//! Records the default microphone and the default output's loopback into two
//! mono Ogg/Opus files, encoded by FFmpeg as the audio comes in.
//!
//! Each track follows the Windows default device: when it changes (a headset
//! connected, Bluetooth switching to its hands-free profile for a call) or the
//! device fails, the track reopens on the new default and carries on in the
//! same file, resampled to the rate the file started with.
//!
//! WASAPI loopback delivers nothing while nothing plays, a full queue drops
//! packets and a device switch leaves a hole. Every packet therefore carries
//! the time it arrived, and the writer pads silence wherever a track fell
//! behind the clock, up to the moment recording stopped, so both tracks stay
//! on the same timeline.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, Stream, SupportedStreamConfig};
use serde::{Deserialize, Serialize};

use crate::types::Side;

const QUEUE_PACKETS: usize = 256;
/// Opus bitrate per (mono) track: transparent enough for speech.
pub const BITRATE_BPS: u32 = 48_000;
/// A track this far behind the clock gets silence.
const GAP_TOLERANCE: Duration = Duration::from_millis(150);
/// How often the default devices are checked.
const WATCH_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackInfo {
    pub file: String,
    /// The devices used, in order; more than one after a switch.
    pub devices: Vec<String>,
    pub sample_rate: u32,
    pub bitrate_bps: u32,
    pub duration_ms: i64,
    /// Silence inserted for loopback pauses, dropped packets and switches.
    pub padded_ms: i64,
    pub dropped_packets: u64,
    pub xruns: u64,
}

/// session.json: what the capture did.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub status: String,
    pub started_at_unix_ms: i64,
    pub stopped_at_unix_ms: Option<i64>,
    pub mic: Option<TrackInfo>,
    pub computer: Option<TrackInfo>,
    pub errors: Vec<String>,
}

impl Session {
    pub fn load(dir: &Path) -> Option<Session> {
        let text = std::fs::read_to_string(dir.join("session.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        let path = dir.join("session.json");
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))
    }
}

pub fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// A packet of mono samples at the track rate and when it arrived, relative
/// to the recording start.
struct Packet {
    at: Duration,
    samples: Vec<i16>,
}

struct WriterResult {
    samples: u64,
    padded: u64,
}

/// State of one track shared by its streams, its supervisor and the recorder.
#[derive(Clone)]
struct TrackState {
    side: Side,
    /// The file's sample rate: the first device's.
    rate: u32,
    started: Instant,
    sender: SyncSender<Packet>,
    peak: Arc<AtomicU32>,
    dropped_packets: Arc<AtomicU64>,
    xruns: Arc<AtomicU64>,
    /// Set by a stream error: the supervisor reopens the device.
    reopen: Arc<AtomicBool>,
    devices: Arc<Mutex<Vec<String>>>,
    errors: Sender<String>,
}

struct Track {
    state: TrackState,
    quit: Sender<()>,
    supervisor: JoinHandle<()>,
    writer: JoinHandle<Result<WriterResult, String>>,
    /// Microseconds since the start at which recording stopped; the writer
    /// pads the track up to it.
    stop_at_us: Arc<AtomicU64>,
}

impl Track {
    fn finish(self, stopped: Duration) -> Result<TrackInfo, String> {
        let Track {
            state,
            quit,
            supervisor,
            writer,
            stop_at_us,
        } = self;
        stop_at_us.store(stopped.as_micros() as u64, Ordering::SeqCst);
        drop(quit);
        let _ = supervisor.join();
        let TrackState {
            side,
            rate,
            sender,
            dropped_packets,
            xruns,
            devices,
            ..
        } = state;
        drop(sender);
        let result = writer
            .join()
            .map_err(|_| format!("{}: encoder thread panicked", side.label()))??;
        let ms = |samples: u64| (samples * 1000 / u64::from(rate)) as i64;
        let devices = devices.lock().map(|d| d.clone()).unwrap_or_default();
        Ok(TrackInfo {
            file: side.file_name().to_owned(),
            devices,
            sample_rate: rate,
            bitrate_bps: BITRATE_BPS,
            duration_ms: ms(result.samples),
            padded_ms: ms(result.padded),
            dropped_packets: dropped_packets.load(Ordering::Relaxed),
            xruns: xruns.load(Ordering::Relaxed),
        })
    }
}

/// A running recording. End it with [Recorder::stop].
pub struct Recorder {
    dir: PathBuf,
    mic: Track,
    computer: Track,
    started: Instant,
    started_at_unix_ms: i64,
    fatal: Arc<AtomicBool>,
    error_receiver: Receiver<String>,
    errors: Vec<String>,
}

impl Recorder {
    /// Starts recording into the folder (created when missing).
    pub fn start(dir: &Path) -> Result<Recorder, String> {
        if [Side::Mic, Side::Computer]
            .iter()
            .any(|side| dir.join(side.file_name()).exists())
        {
            return Err(format!("{} already holds a recording", dir.display()));
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        crate::ffmpeg::check()?;

        let (error_sender, error_receiver) = mpsc::channel();
        let fatal = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let mic = start_track(Side::Mic, dir, started, Arc::clone(&fatal), error_sender.clone())?;
        let computer = match start_track(
            Side::Computer,
            dir,
            started,
            Arc::clone(&fatal),
            error_sender,
        ) {
            Ok(track) => track,
            Err(e) => {
                let _ = mic.finish(started.elapsed());
                return Err(e);
            }
        };
        let started_at_unix_ms = unix_ms();
        Session {
            status: "recording".into(),
            started_at_unix_ms,
            ..Default::default()
        }
        .save(dir)?;
        Ok(Recorder {
            dir: dir.to_path_buf(),
            mic,
            computer,
            started,
            started_at_unix_ms,
            fatal,
            error_receiver,
            errors: Vec::new(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn started_at_unix_ms(&self) -> i64 {
        self.started_at_unix_ms
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The device the microphone track records from right now.
    pub fn mic_device(&self) -> String {
        current_device(&self.mic.state)
    }

    /// The output whose loopback the computer track records right now.
    pub fn computer_device(&self) -> String {
        current_device(&self.computer.state)
    }

    /// Peak level of (mic, computer) since the previous call, 0.0..=1.0.
    pub fn levels(&self) -> (f32, f32) {
        let take =
            |peak: &AtomicU32| f32::from_bits(peak.swap(0, Ordering::Relaxed)).clamp(0.0, 1.0);
        (take(&self.mic.state.peak), take(&self.computer.state.peak))
    }

    /// Xrun counts of (mic, computer).
    pub fn xruns(&self) -> (u64, u64) {
        (
            self.mic.state.xruns.load(Ordering::Relaxed),
            self.computer.state.xruns.load(Ordering::Relaxed),
        )
    }

    /// Messages so far (device switches and errors), and whether one was
    /// fatal, which means the recording should be stopped.
    pub fn poll_errors(&mut self) -> (&[String], bool) {
        while let Ok(error) = self.error_receiver.try_recv() {
            self.errors.push(error);
        }
        (&self.errors, self.fatal.load(Ordering::Relaxed))
    }

    /// Stops both tracks, finalizes the files and writes session.json.
    pub fn stop(mut self) -> Result<Session, String> {
        let stopped_at = unix_ms();
        let stopped = self.started.elapsed();
        self.poll_errors();
        let mut errors = std::mem::take(&mut self.errors);
        let mic = self.mic.finish(stopped).map_err(|e| errors.push(e)).ok();
        let computer = self.computer.finish(stopped).map_err(|e| errors.push(e)).ok();
        while let Ok(error) = self.error_receiver.try_recv() {
            errors.push(error);
        }
        let failed = self.fatal.load(Ordering::Relaxed) || mic.is_none() || computer.is_none();
        let session = Session {
            status: if failed { "failed" } else { "completed" }.into(),
            started_at_unix_ms: self.started_at_unix_ms,
            stopped_at_unix_ms: Some(stopped_at),
            mic,
            computer,
            errors,
        };
        session.save(&self.dir)?;
        Ok(session)
    }
}

fn current_device(state: &TrackState) -> String {
    state
        .devices
        .lock()
        .ok()
        .and_then(|d| d.last().cloned())
        .unwrap_or_default()
}

/// The current Windows default device for a side, with its format and a key
/// to notice a change. On WASAPI an input stream on a render endpoint is
/// loopback capture.
fn default_device(side: Side) -> Result<(Device, SupportedStreamConfig, String), String> {
    let host = cpal::default_host();
    let (device, config) = match side {
        Side::Mic => {
            let device = host.default_input_device().ok_or("no default microphone")?;
            let config = device
                .default_input_config()
                .map_err(|e| format!("could not read the microphone format: {e}"))?;
            (device, config)
        }
        Side::Computer => {
            let device = host.default_output_device().ok_or("no default output device")?;
            let config = device
                .default_output_config()
                .map_err(|e| format!("could not read the output format: {e}"))?;
            (device, config)
        }
    };
    let name = device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| side.label().to_owned());
    let key = format!("{:?}|{name}", device.id().ok());
    Ok((device, config, key))
}

fn device_name(key: &str) -> &str {
    key.split_once('|').map_or(key, |(_, name)| name)
}

fn start_track(
    side: Side,
    dir: &Path,
    started: Instant,
    fatal: Arc<AtomicBool>,
    errors: Sender<String>,
) -> Result<Track, String> {
    let (device, config, key) = default_device(side)?;
    let rate = config.sample_rate();
    let (sender, receiver) = mpsc::sync_channel(QUEUE_PACKETS);
    let stop_at_us = Arc::new(AtomicU64::new(u64::MAX));
    let writer = spawn_writer(
        dir.join(side.file_name()),
        rate,
        receiver,
        Arc::clone(&stop_at_us),
    )?;
    let state = TrackState {
        side,
        rate,
        started,
        sender,
        peak: Arc::new(AtomicU32::new(0)),
        dropped_packets: Arc::new(AtomicU64::new(0)),
        xruns: Arc::new(AtomicU64::new(0)),
        reopen: Arc::new(AtomicBool::new(false)),
        devices: Arc::new(Mutex::new(Vec::new())),
        errors,
    };
    let (quit, quit_receiver) = mpsc::channel::<()>();
    let (ready, ready_receiver) = mpsc::channel::<Result<(), String>>();
    let supervised = state.clone();
    let supervisor = thread::Builder::new()
        .name(format!("capture-{}", side.label()))
        .spawn(move || supervise(supervised, device, config, key, fatal, ready, quit_receiver))
        .map_err(|e| format!("could not start the capture thread: {e}"))?;
    let track = Track {
        state,
        quit,
        supervisor,
        writer,
        stop_at_us,
    };
    match ready_receiver.recv() {
        Ok(Ok(())) => Ok(track),
        Ok(Err(e)) => {
            let _ = track.finish(Duration::ZERO);
            Err(e)
        }
        Err(_) => Err(format!("{}: the capture thread stopped", side.label())),
    }
}

/// Owns the track's stream (a stream is dropped on the thread that made it)
/// and reopens it on the current default device whenever that changes or the
/// stream fails.
fn supervise(
    state: TrackState,
    device: Device,
    config: SupportedStreamConfig,
    key: String,
    fatal: Arc<AtomicBool>,
    ready: Sender<Result<(), String>>,
    quit: Receiver<()>,
) {
    let label = state.side.label();
    let mut current = match open(&state, &device, config) {
        Ok(stream) => {
            push_device(&state, &key);
            let _ = ready.send(Ok(()));
            Some((key, stream))
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut failing_since: Option<Instant> = None;
    loop {
        match quit.recv_timeout(WATCH_INTERVAL) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => break,
        }
        let reopen = state.reopen.swap(false, Ordering::Relaxed);
        let wanted = default_device(state.side);
        let changed = match (&wanted, &current) {
            (Ok((_, _, key)), Some((current_key, _))) => key != current_key,
            (Ok(_), None) => true,
            (Err(_), _) => false,
        };
        if !reopen && !changed {
            continue;
        }
        drop(current.take());
        match wanted.and_then(|(device, config, key)| {
            open(&state, &device, config).map(|stream| (key, stream))
        }) {
            Ok((key, stream)) => {
                let _ = state
                    .errors
                    .send(format!("{label}: now recording {}", device_name(&key)));
                push_device(&state, &key);
                current = Some((key, stream));
                failing_since = None;
            }
            Err(e) => {
                // Keep trying; give up after a minute without any device.
                let since = *failing_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(60) {
                    let _ = state.errors.send(format!("{label}: {e}"));
                    fatal.store(true, Ordering::Relaxed);
                    break;
                }
                state.reopen.store(true, Ordering::Relaxed);
            }
        }
    }
    drop(current);
}

fn push_device(state: &TrackState, key: &str) {
    if let Ok(mut devices) = state.devices.lock() {
        devices.push(device_name(key).to_owned());
    }
}

/// Pipes mono PCM into FFmpeg, padding silence where packets are missing and
/// at the end up to the stop time.
fn spawn_writer(
    path: PathBuf,
    rate: u32,
    receiver: Receiver<Packet>,
    stop_at_us: Arc<AtomicU64>,
) -> Result<JoinHandle<Result<WriterResult, String>>, String> {
    let mut child = spawn_encoder(&path, rate)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("FFmpeg gave no input pipe for {}", path.display()))?;
    thread::Builder::new()
        .name(format!("encoder-{}", path.display()))
        .spawn(move || {
            let rate = u64::from(rate);
            let at_sample = |us: u64| us * rate / 1_000_000;
            let tolerance = at_sample(GAP_TOLERANCE.as_micros() as u64);
            let mut written = 0u64;
            let mut padded = 0u64;
            let mut failure = None;
            while let Ok(packet) = receiver.recv() {
                let len = packet.samples.len() as u64;
                // The packet ends at its arrival time, so it starts len earlier.
                let expected_start = at_sample(packet.at.as_micros() as u64).saturating_sub(len);
                if expected_start > written + tolerance {
                    let gap = expected_start - written;
                    if let Err(e) = write_silence(&mut stdin, gap) {
                        failure = Some(e);
                        break;
                    }
                    written += gap;
                    padded += gap;
                }
                if let Err(e) = write_samples(&mut stdin, &packet.samples) {
                    failure = Some(e);
                    break;
                }
                written += len;
            }
            let stop_at = stop_at_us.load(Ordering::SeqCst);
            if failure.is_none() && stop_at != u64::MAX {
                let end = at_sample(stop_at);
                if end > written + tolerance {
                    let gap = end - written;
                    match write_silence(&mut stdin, gap) {
                        Ok(()) => {
                            written += gap;
                            padded += gap;
                        }
                        Err(e) => failure = Some(e),
                    }
                }
            }
            finish_encoder(child, stdin, &path, failure)?;
            Ok(WriterResult {
                samples: written,
                padded,
            })
        })
        .map_err(|e| format!("could not start the encoder thread: {e}"))
}

fn spawn_encoder(path: &Path, rate: u32) -> Result<Child, String> {
    crate::ffmpeg::command()
        .args(["-y", "-f", "s16le", "-ar", &rate.to_string(), "-ac", "1"])
        .args(["-i", "pipe:0", "-map_metadata", "-1", "-c:a", "libopus"])
        .args(["-b:a", &BITRATE_BPS.to_string(), "-vbr", "on", "-application", "voip"])
        .args(["-f", "ogg"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start FFmpeg for {}: {e}", path.display()))
}

fn write_samples(stdin: &mut ChildStdin, samples: &[i16]) -> Result<(), String> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    stdin
        .write_all(&bytes)
        .map_err(|e| format!("could not stream audio to FFmpeg: {e}"))
}

fn write_silence(stdin: &mut ChildStdin, mut samples: u64) -> Result<(), String> {
    let zeros = vec![0u8; 64 * 1024];
    while samples > 0 {
        let n = samples.min(zeros.len() as u64 / 2);
        stdin
            .write_all(&zeros[..n as usize * 2])
            .map_err(|e| format!("could not stream audio to FFmpeg: {e}"))?;
        samples -= n;
    }
    Ok(())
}

fn finish_encoder(
    mut child: Child,
    stdin: ChildStdin,
    path: &Path,
    failure: Option<String>,
) -> Result<(), String> {
    drop(stdin);
    let status = child
        .wait()
        .map_err(|e| format!("could not finalize {}: {e}", path.display()))?;
    if let Some(error) = failure {
        return Err(error);
    }
    if !status.success() {
        return Err(format!("FFmpeg failed to encode {} ({status})", path.display()));
    }
    Ok(())
}

fn pcm16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Linear resampling of a mono stream from the device rate to the file rate;
/// plenty for speech, and only used after a switch to a device with another
/// rate.
struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Position of the next output sample between prev (0) and the next input (1).
    t: f64,
    prev: f32,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Option<Resampler> {
        (from != to).then(|| Resampler {
            step: f64::from(from) / f64::from(to),
            t: 1.0,
            prev: 0.0,
        })
    }

    fn push(&mut self, x: f32, out: &mut Vec<i16>) {
        while self.t <= 1.0 {
            out.push(pcm16(self.prev + (x - self.prev) * self.t as f32));
            self.t += self.step;
        }
        self.t -= 1.0;
        self.prev = x;
    }
}

fn build_stream<T>(
    state: &TrackState,
    device: &Device,
    config: SupportedStreamConfig,
) -> Result<Stream, String>
where
    T: SizedSample + Copy,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels()).max(1);
    let mut resampler = Resampler::new(config.sample_rate(), state.rate);
    let data_state = state.clone();
    let error_state = state.clone();
    device
        .build_input_stream(
            config.config(),
            move |data: &[T], _| {
                let s = &data_state;
                let at = s.started.elapsed();
                let mut samples = Vec::with_capacity(data.len() / channels + 8);
                let mut level = 0.0f32;
                for frame in data.chunks(channels) {
                    let sum: f32 = frame.iter().map(|x| f32::from_sample(*x)).sum();
                    let mono = sum / channels as f32;
                    level = level.max(mono.abs());
                    match resampler.as_mut() {
                        Some(r) => r.push(mono, &mut samples),
                        None => samples.push(pcm16(mono)),
                    }
                }
                s.peak.fetch_max(level.to_bits(), Ordering::Relaxed);
                if let Err(TrySendError::Full(_)) = s.sender.try_send(Packet { at, samples }) {
                    s.dropped_packets.fetch_add(1, Ordering::Relaxed);
                }
            },
            move |error| {
                let s = &error_state;
                if error.kind() == cpal::ErrorKind::Xrun {
                    s.xruns.fetch_add(1, Ordering::Relaxed);
                } else if !s.reopen.swap(true, Ordering::Relaxed) {
                    let _ = s.errors.send(format!("{}: {error}", s.side.label()));
                }
            },
            None,
        )
        .map_err(|e| format!("could not open the {} stream: {e}", state.side.label()))
}

fn open(state: &TrackState, device: &Device, config: SupportedStreamConfig) -> Result<Stream, String> {
    let stream = match config.sample_format() {
        SampleFormat::F32 => build_stream::<f32>(state, device, config),
        SampleFormat::F64 => build_stream::<f64>(state, device, config),
        SampleFormat::I8 => build_stream::<i8>(state, device, config),
        SampleFormat::I16 => build_stream::<i16>(state, device, config),
        SampleFormat::I32 => build_stream::<i32>(state, device, config),
        SampleFormat::U8 => build_stream::<u8>(state, device, config),
        SampleFormat::U16 => build_stream::<u16>(state, device, config),
        SampleFormat::U32 => build_stream::<u32>(state, device, config),
        other => Err(format!("unsupported sample format: {other:?}")),
    }?;
    stream
        .play()
        .map_err(|e| format!("could not start the {} stream: {e}", state.side.label()))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_the_duration() {
        let mut r = Resampler::new(44_100, 48_000).unwrap();
        let mut out = Vec::new();
        for i in 0..44_100 {
            r.push((i as f32 * 0.01).sin() * 0.5, &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() <= 2, "{}", out.len());
        assert!(Resampler::new(48_000, 48_000).is_none());
    }
}
