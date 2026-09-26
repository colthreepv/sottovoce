//! Records the default microphone and the default output's loopback into two
//! mono Ogg/Opus files, encoded by FFmpeg as the audio comes in.
//!
//! WASAPI loopback delivers nothing while nothing plays, and a full queue
//! drops packets. Every packet therefore carries the time it arrived, and the
//! writer pads silence wherever a track fell behind the clock, so both tracks
//! stay on the same timeline as the wall clock.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackInfo {
    pub file: String,
    pub device: String,
    pub input_sample_rate: u32,
    pub input_channels: u16,
    pub bitrate_bps: u32,
    pub duration_ms: i64,
    /// Silence inserted for loopback pauses and dropped packets.
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

/// A packet of mono samples and when it arrived, relative to the recording start.
struct Packet {
    at: Duration,
    samples: Vec<i16>,
}

struct WriterResult {
    samples: u64,
    padded: u64,
}

struct Track {
    side: Side,
    device_name: String,
    stream: Stream,
    sender: SyncSender<Packet>,
    writer: JoinHandle<Result<WriterResult, String>>,
    peak: Arc<AtomicU32>,
    dropped_packets: Arc<AtomicU64>,
    xruns: Arc<AtomicU64>,
    sample_rate: u32,
    channels: u16,
}

impl Track {
    fn finish(self) -> Result<TrackInfo, String> {
        let Track {
            side,
            device_name,
            stream,
            sender,
            writer,
            dropped_packets,
            xruns,
            sample_rate,
            channels,
            ..
        } = self;
        drop(stream);
        drop(sender);
        let result = writer
            .join()
            .map_err(|_| format!("{}: encoder thread panicked", side.label()))??;
        let ms = |samples: u64| (samples * 1000 / u64::from(sample_rate)) as i64;
        Ok(TrackInfo {
            file: side.file_name().to_owned(),
            device: device_name,
            input_sample_rate: sample_rate,
            input_channels: channels,
            bitrate_bps: BITRATE_BPS,
            duration_ms: ms(result.samples),
            padded_ms: ms(result.padded),
            dropped_packets: dropped_packets.load(Ordering::Relaxed),
            xruns: xruns.load(Ordering::Relaxed),
        })
    }
}

/// A running recording. Drop it only through [Recorder::stop].
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
    /// Starts recording into `dir` (created when missing).
    pub fn start(dir: &Path) -> Result<Recorder, String> {
        if [Side::Mic, Side::Computer]
            .iter()
            .any(|side| dir.join(side.file_name()).exists())
        {
            return Err(format!("{} already holds a recording", dir.display()));
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        crate::ffmpeg::check()?;

        let host = cpal::default_host();
        let microphone = host
            .default_input_device()
            .ok_or("no default microphone found")?;
        let output = host
            .default_output_device()
            .ok_or("no default output device found")?;
        let mic_config = microphone
            .default_input_config()
            .map_err(|e| format!("could not read the microphone format: {e}"))?;
        // On WASAPI an input stream on a render endpoint is loopback capture.
        let output_config = output
            .default_output_config()
            .map_err(|e| format!("could not read the output format: {e}"))?;

        let (error_sender, error_receiver) = mpsc::channel();
        let fatal = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let mic = start_track(
            Side::Mic,
            &microphone,
            mic_config,
            dir,
            started,
            Arc::clone(&fatal),
            error_sender.clone(),
        )?;
        let computer = start_track(
            Side::Computer,
            &output,
            output_config,
            dir,
            started,
            Arc::clone(&fatal),
            error_sender,
        )?;
        let started_at_unix_ms = unix_ms();
        let recorder = Recorder {
            dir: dir.to_path_buf(),
            mic,
            computer,
            started,
            started_at_unix_ms,
            fatal,
            error_receiver,
            errors: Vec::new(),
        };
        let playing = [&recorder.mic, &recorder.computer]
            .into_iter()
            .try_for_each(|track| {
                track
                    .stream
                    .play()
                    .map_err(|e| format!("could not start {}: {e}", track.side.label()))
            });
        if let Err(message) = playing {
            let _ = recorder.stop();
            return Err(message);
        }
        Session {
            status: "recording".into(),
            started_at_unix_ms,
            ..Default::default()
        }
        .save(dir)?;
        Ok(recorder)
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

    pub fn mic_device(&self) -> &str {
        &self.mic.device_name
    }

    pub fn computer_device(&self) -> &str {
        &self.computer.device_name
    }

    /// Peak level of (mic, computer) since the previous call, 0.0..=1.0.
    pub fn levels(&self) -> (f32, f32) {
        let take = |peak: &AtomicU32| f32::from_bits(peak.swap(0, Ordering::Relaxed)).clamp(0.0, 1.0);
        (take(&self.mic.peak), take(&self.computer.peak))
    }

    /// Xrun counts of (mic, computer).
    pub fn xruns(&self) -> (u64, u64) {
        (
            self.mic.xruns.load(Ordering::Relaxed),
            self.computer.xruns.load(Ordering::Relaxed),
        )
    }

    /// Errors so far; a fatal one means the recording should be stopped.
    pub fn poll_errors(&mut self) -> (&[String], bool) {
        while let Ok(error) = self.error_receiver.try_recv() {
            self.errors.push(error);
        }
        (&self.errors, self.fatal.load(Ordering::Relaxed))
    }

    /// Stops both streams, finalizes the files and writes session.json.
    pub fn stop(mut self) -> Result<Session, String> {
        let stopped_at = unix_ms();
        self.poll_errors();
        let failed_before = self.fatal.load(Ordering::Relaxed);
        let mut errors = std::mem::take(&mut self.errors);
        let mic = self.mic.finish().map_err(|e| errors.push(e.clone())).ok();
        let computer = self.computer.finish().map_err(|e| errors.push(e.clone())).ok();
        while let Ok(error) = self.error_receiver.try_recv() {
            errors.push(error);
        }
        let failed = failed_before || mic.is_none() || computer.is_none();
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

fn start_track(
    side: Side,
    device: &Device,
    config: SupportedStreamConfig,
    dir: &Path,
    started: Instant,
    fatal: Arc<AtomicBool>,
    error_sender: Sender<String>,
) -> Result<Track, String> {
    let sample_rate = config.sample_rate();
    let channels = config.channels();
    let device_name = device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| side.label().to_owned());
    let (sender, receiver) = mpsc::sync_channel(QUEUE_PACKETS);
    let writer = spawn_writer(dir.join(side.file_name()), sample_rate, receiver)?;
    let peak = Arc::new(AtomicU32::new(0));
    let dropped_packets = Arc::new(AtomicU64::new(0));
    let xruns = Arc::new(AtomicU64::new(0));
    let shared = Shared {
        side,
        channels: usize::from(channels),
        started,
        sender: sender.clone(),
        peak: Arc::clone(&peak),
        dropped_packets: Arc::clone(&dropped_packets),
        xruns: Arc::clone(&xruns),
        fatal,
        error_sender,
    };
    let stream = build_stream_for_format(device, config, shared)?;
    Ok(Track {
        side,
        device_name,
        stream,
        sender,
        writer,
        peak,
        dropped_packets,
        xruns,
        sample_rate,
        channels,
    })
}

/// Pipes mono PCM into FFmpeg, padding silence where packets are missing.
fn spawn_writer(
    path: PathBuf,
    sample_rate: u32,
    receiver: Receiver<Packet>,
) -> Result<JoinHandle<Result<WriterResult, String>>, String> {
    let mut child = spawn_encoder(&path, sample_rate)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("FFmpeg gave no input pipe for {}", path.display()))?;
    thread::Builder::new()
        .name(format!("encoder-{}", path.display()))
        .spawn(move || {
            let rate = u64::from(sample_rate);
            let tolerance = GAP_TOLERANCE.as_micros() as u64 * rate / 1_000_000;
            let mut written = 0u64;
            let mut padded = 0u64;
            let mut failure = None;
            while let Ok(packet) = receiver.recv() {
                let len = packet.samples.len() as u64;
                // The packet ends at its arrival time, so it starts len earlier.
                let expected_start =
                    (packet.at.as_micros() as u64 * rate / 1_000_000).saturating_sub(len);
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
            finish_encoder(child, stdin, &path, failure)?;
            Ok(WriterResult {
                samples: written,
                padded,
            })
        })
        .map_err(|e| format!("could not start the encoder thread: {e}"))
}

fn spawn_encoder(path: &Path, sample_rate: u32) -> Result<Child, String> {
    crate::ffmpeg::command()
        .args(["-y", "-f", "s16le", "-ar", &sample_rate.to_string(), "-ac", "1"])
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

/// What the audio callbacks share with the rest.
struct Shared {
    side: Side,
    channels: usize,
    started: Instant,
    sender: SyncSender<Packet>,
    peak: Arc<AtomicU32>,
    dropped_packets: Arc<AtomicU64>,
    xruns: Arc<AtomicU64>,
    fatal: Arc<AtomicBool>,
    error_sender: Sender<String>,
}

fn pcm16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

fn build_stream<T>(
    device: &Device,
    config: SupportedStreamConfig,
    shared: Shared,
) -> Result<Stream, String>
where
    T: SizedSample + Copy,
    f32: FromSample<T>,
{
    let Shared {
        side,
        channels,
        started,
        sender,
        peak,
        dropped_packets,
        xruns,
        fatal,
        error_sender,
    } = shared;
    let channels = channels.max(1);
    let callback_fatal = Arc::clone(&fatal);
    let callback_errors = error_sender.clone();
    device
        .build_input_stream(
            config.config(),
            move |data: &[T], _| {
                let at = started.elapsed();
                let mut samples = Vec::with_capacity(data.len() / channels);
                let mut level = 0.0f32;
                for frame in data.chunks(channels) {
                    let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
                    let mono = sum / channels as f32;
                    level = level.max(mono.abs());
                    samples.push(pcm16(mono));
                }
                peak.fetch_max(level.to_bits(), Ordering::Relaxed);
                match sender.try_send(Packet { at, samples }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        dropped_packets.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        if !callback_fatal.swap(true, Ordering::Relaxed) {
                            let _ = callback_errors
                                .send(format!("{}: the encoder stopped", side.label()));
                        }
                    }
                }
            },
            move |error| {
                if error.kind() == cpal::ErrorKind::Xrun {
                    xruns.fetch_add(1, Ordering::Relaxed);
                } else {
                    fatal.store(true, Ordering::Relaxed);
                    let _ = error_sender.send(format!("{} capture error: {error}", side.label()));
                }
            },
            None,
        )
        .map_err(|e| format!("could not open the {} stream: {e}", side.label()))
}

fn build_stream_for_format(
    device: &Device,
    config: SupportedStreamConfig,
    shared: Shared,
) -> Result<Stream, String> {
    match config.sample_format() {
        SampleFormat::F32 => build_stream::<f32>(device, config, shared),
        SampleFormat::F64 => build_stream::<f64>(device, config, shared),
        SampleFormat::I8 => build_stream::<i8>(device, config, shared),
        SampleFormat::I16 => build_stream::<i16>(device, config, shared),
        SampleFormat::I32 => build_stream::<i32>(device, config, shared),
        SampleFormat::U8 => build_stream::<u8>(device, config, shared),
        SampleFormat::U16 => build_stream::<u16>(device, config, shared),
        SampleFormat::U32 => build_stream::<u32>(device, config, shared),
        other => Err(format!("unsupported sample format: {other:?}")),
    }
}
