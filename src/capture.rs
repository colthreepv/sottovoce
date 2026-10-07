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

use crate::devices::{self, DeviceChoice};
use crate::types::{DeviceChange, Side};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Error,
}
#[derive(Clone, Copy, Debug)]
enum DeviceNoticeKind {
    DefaultSwitched,
    PinnedReturned,
    PinnedMissing,
}
impl DeviceNoticeKind {
    fn level(self) -> NoticeLevel {
        match self {
            Self::DefaultSwitched | Self::PinnedReturned => NoticeLevel::Info,
            Self::PinnedMissing => NoticeLevel::Error,
        }
    }
}

/// Typed at the source; consumers never infer severity from message text.
#[derive(Clone, Debug)]
pub struct CaptureNotice {
    pub level: NoticeLevel,
    pub message: String,
    pub change: Option<DeviceChange>,
}
impl CaptureNotice {
    fn error(message: String) -> Self {
        Self {
            level: NoticeLevel::Error,
            message,
            change: None,
        }
    }
    fn device(
        kind: DeviceNoticeKind,
        message: String,
        side: Side,
        device: &str,
        at_ms: u64,
    ) -> Self {
        Self {
            level: kind.level(),
            message,
            change: Some(DeviceChange {
                at_ms,
                side,
                device: device.to_owned(),
            }),
        }
    }
}

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
    pub device_changes: Vec<DeviceChange>,
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
    errors: Sender<CaptureNotice>,
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
    error_receiver: Receiver<CaptureNotice>,
    errors: Vec<String>,
    notices: Vec<CaptureNotice>,
    device_changes: Vec<DeviceChange>,
}

/// Lightweight live level monitor. Streams and supervisors are joined on drop.
pub struct Monitor {
    mic: TrackState,
    computer: TrackState,
    mic_receiver: Receiver<Packet>,
    computer_receiver: Receiver<Packet>,
    workers: Vec<(Sender<()>, JoinHandle<()>)>,
    error_receiver: Receiver<CaptureNotice>,
}

impl Monitor {
    pub fn start(mic_choice: DeviceChoice, output_choice: DeviceChoice) -> Result<Self, String> {
        let started = Instant::now();
        let (errors, error_receiver) = mpsc::channel();
        let (mic, mic_receiver, mic_worker) =
            start_monitor_track(Side::Mic, started, errors.clone(), mic_choice)?;
        let (computer, computer_receiver, computer_worker) =
            match start_monitor_track(Side::Computer, started, errors, output_choice) {
                Ok(track) => track,
                Err(error) => {
                    stop_monitor_worker(mic_worker);
                    return Err(error);
                }
            };
        Ok(Self {
            mic,
            computer,
            mic_receiver,
            computer_receiver,
            workers: vec![mic_worker, computer_worker],
            error_receiver,
        })
    }

    pub fn poll_notices(&self) -> Vec<CaptureNotice> {
        self.error_receiver.try_iter().collect()
    }
    pub fn poll_errors(&self) -> Vec<String> {
        self.poll_notices()
            .into_iter()
            .filter(|n| n.level == NoticeLevel::Error)
            .map(|n| n.message)
            .collect()
    }

    pub fn levels(&self) -> (f32, f32) {
        while self.mic_receiver.try_recv().is_ok() {}
        while self.computer_receiver.try_recv().is_ok() {}
        let take =
            |peak: &AtomicU32| f32::from_bits(peak.swap(0, Ordering::Relaxed)).clamp(0.0, 1.0);
        (take(&self.mic.peak), take(&self.computer.peak))
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        for worker in self.workers.drain(..) {
            stop_monitor_worker(worker);
        }
    }
}

fn stop_monitor_worker(worker: (Sender<()>, JoinHandle<()>)) {
    drop(worker.0);
    let _ = worker.1.join();
}

fn start_monitor_track(
    side: Side,
    started: Instant,
    errors: Sender<CaptureNotice>,
    choice: DeviceChoice,
) -> Result<(TrackState, Receiver<Packet>, (Sender<()>, JoinHandle<()>)), String> {
    let (_, config, _, _) = resolve_device(side, &choice)?;
    let rate = config.sample_rate();
    let (sender, receiver) = mpsc::sync_channel(QUEUE_PACKETS);
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
    let (quit, quit_receiver) = mpsc::channel();
    let (ready, ready_receiver) = mpsc::channel();
    let supervisor_state = state.clone();
    let supervisor = thread::Builder::new()
        .name(format!("monitor-{}", side.label()))
        .spawn(move || {
            supervise(
                supervisor_state,
                choice,
                Arc::new(AtomicBool::new(false)),
                ready,
                quit_receiver,
            )
        })
        .map_err(|e| format!("could not start the monitor thread: {e}"))?;
    match ready_receiver.recv() {
        Ok(Ok(())) => Ok((state, receiver, (quit, supervisor))),
        Ok(Err(error)) => {
            drop(quit);
            let _ = supervisor.join();
            Err(error)
        }
        Err(_) => {
            drop(quit);
            let _ = supervisor.join();
            Err(format!("{}: the monitor thread stopped", side.label()))
        }
    }
}

impl Recorder {
    /// Starts recording into the folder (created when missing).
    pub fn start(
        dir: &Path,
        mic_choice: DeviceChoice,
        output_choice: DeviceChoice,
    ) -> Result<Recorder, String> {
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
        let mic = start_track(
            Side::Mic,
            dir,
            started,
            Arc::clone(&fatal),
            error_sender.clone(),
            mic_choice,
        )?;
        let computer = match start_track(
            Side::Computer,
            dir,
            started,
            Arc::clone(&fatal),
            error_sender,
            output_choice,
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
            notices: Vec::new(),
            device_changes: Vec::new(),
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
            if error.level == NoticeLevel::Error {
                self.errors.push(error.message.clone());
            }
            if let Some(change) = &error.change {
                self.device_changes.push(change.clone());
            }
            self.notices.push(error);
        }
        (&self.errors, self.fatal.load(Ordering::Relaxed))
    }

    pub fn poll_notices(&mut self) -> (Vec<CaptureNotice>, bool) {
        self.poll_errors();
        (
            std::mem::take(&mut self.notices),
            self.fatal.load(Ordering::Relaxed),
        )
    }

    /// Stops both tracks, finalizes the files and writes session.json.
    pub fn stop(mut self) -> Result<Session, String> {
        let stopped_at = unix_ms();
        let stopped = self.started.elapsed();
        self.poll_errors();
        let mut errors = std::mem::take(&mut self.errors);
        let mic = self.mic.finish(stopped).map_err(|e| errors.push(e)).ok();
        let computer = self
            .computer
            .finish(stopped)
            .map_err(|e| errors.push(e))
            .ok();
        while let Ok(error) = self.error_receiver.try_recv() {
            if error.level == NoticeLevel::Error {
                errors.push(error.message);
            }
            if let Some(change) = error.change {
                self.device_changes.push(change);
            }
        }
        let failed = self.fatal.load(Ordering::Relaxed) || mic.is_none() || computer.is_none();
        let session = Session {
            status: if failed { "failed" } else { "completed" }.into(),
            started_at_unix_ms: self.started_at_unix_ms,
            stopped_at_unix_ms: Some(stopped_at),
            mic,
            computer,
            errors,
            device_changes: self.device_changes,
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
            let device = host
                .default_output_device()
                .ok_or("no default output device")?;
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
    let key = format!("{}|{name}", device.id().map_err(|e| e.to_string())?);
    Ok((device, config, key))
}

fn resolve_device(
    side: Side,
    choice: &DeviceChoice,
) -> Result<(Device, SupportedStreamConfig, String, bool), String> {
    let pinned = if let DeviceChoice::Pinned(id) = choice {
        devices::find(id).and_then(|device| {
            let config = match side {
                Side::Mic => device.default_input_config(),
                Side::Computer => device.default_output_config(),
            }
            .ok()?;
            let name = device
                .description()
                .map(|d| d.name().to_owned())
                .unwrap_or_else(|_| device.to_string());
            let key = format!("{}|{name}", device.id().ok()?);
            Some((device, config, key))
        })
    } else {
        None
    };
    let pinned_available = pinned.is_some();
    let default = default_device(side).ok();
    devices::choose_pinned(choice, pinned, default, pinned_available)
        .map(|((device, config, key), pinned)| (device, config, key, pinned))
        .ok_or_else(|| format!("no usable {} device", side.label()))
}

fn open_choice(
    state: &TrackState,
    choice: &DeviceChoice,
) -> Result<(String, Stream, bool), String> {
    let (device, config, key, pinned) = resolve_device(state.side, choice)?;
    match open(state, &device, config) {
        Ok(stream) => Ok((key, stream, pinned)),
        Err(pinned_error) if pinned => {
            let (default, config, key) = default_device(state.side)?;
            open(state, &default, config)
                .map(|stream| (key, stream, false))
                .map_err(|default_error| {
                    format!("{pinned_error}; Windows default also failed: {default_error}")
                })
        }
        Err(error) => Err(error),
    }
}

fn device_name(key: &str) -> &str {
    key.split_once('|').map_or(key, |(_, name)| name)
}

fn device_label(side: Side) -> &'static str {
    match side {
        Side::Mic => "Microphone",
        Side::Computer => "Output",
    }
}

fn start_track(
    side: Side,
    dir: &Path,
    started: Instant,
    fatal: Arc<AtomicBool>,
    errors: Sender<CaptureNotice>,
    choice: DeviceChoice,
) -> Result<Track, String> {
    let (_, config, _, _) = resolve_device(side, &choice)?;
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
        .spawn(move || supervise(supervised, choice, fatal, ready, quit_receiver))
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
    choice: DeviceChoice,
    fatal: Arc<AtomicBool>,
    ready: Sender<Result<(), String>>,
    quit: Receiver<()>,
) {
    crate::audio_thread_init();
    let label = state.side.label();
    let mut pinned;
    let mut current = match open_choice(&state, &choice) {
        Ok((resolved_key, stream, resolved_pinned)) => {
            pinned = resolved_pinned;
            let key = resolved_key;
            push_device(&state, &key);
            let _ = ready.send(Ok(()));
            Some((key, stream))
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if let DeviceChoice::Pinned(id) = &choice {
        if !pinned {
            let name = devices::find(id)
                .and_then(|d| d.description().ok().map(|d| d.name().to_owned()))
                .unwrap_or_else(|| id.clone());
            let device = device_name(current.as_ref().map_or("", |(key, _)| key));
            let _ = state.errors.send(CaptureNotice::device(
                DeviceNoticeKind::PinnedMissing,
                format!(
                    "{} {name} not available, using Windows default {}",
                    device_label(state.side),
                    device
                ),
                state.side,
                device,
                state.started.elapsed().as_millis() as u64,
            ));
        }
    }
    let mut failing_since: Option<Instant> = None;
    loop {
        match quit.recv_timeout(WATCH_INTERVAL) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => break,
        }
        let reopen = state.reopen.swap(false, Ordering::Relaxed);
        let wanted = resolve_device(state.side, &choice);
        let changed = match (&wanted, &current) {
            (Ok((_, _, key, _)), Some((current_key, _))) => key != current_key,
            (Ok(_), None) => true,
            (Err(_), _) => true,
        };
        if !reopen && !changed {
            continue;
        }
        let previous_key = current
            .as_ref()
            .map(|(key, _)| key.clone())
            .unwrap_or_default();
        drop(current.take());
        match wanted.and_then(|_| open_choice(&state, &choice)) {
            Ok((key, stream, wanted_pinned)) => {
                if key != previous_key {
                    if let DeviceChoice::Pinned(id) = &choice {
                        if wanted_pinned {
                            let _ = state.errors.send(CaptureNotice::device(
                                DeviceNoticeKind::PinnedReturned,
                                format!(
                                    "{label}: pinned device {} is available again",
                                    device_name(&key)
                                ),
                                state.side,
                                device_name(&key),
                                state.started.elapsed().as_millis() as u64,
                            ));
                        } else if pinned {
                            let name = devices::find(id)
                                .and_then(|d| d.description().ok().map(|d| d.name().to_owned()))
                                .unwrap_or_else(|| id.clone());
                            let _ = state.errors.send(CaptureNotice::device(
                                DeviceNoticeKind::PinnedMissing,
                                format!(
                                    "{} {name} not available, using Windows default {}",
                                    device_label(state.side),
                                    device_name(&key)
                                ),
                                state.side,
                                device_name(&key),
                                state.started.elapsed().as_millis() as u64,
                            ));
                        }
                    } else {
                        let _ = state.errors.send(CaptureNotice::device(
                            DeviceNoticeKind::DefaultSwitched,
                            format!("{label}: device switched to {}", device_name(&key)),
                            state.side,
                            device_name(&key),
                            state.started.elapsed().as_millis() as u64,
                        ));
                    }
                }
                push_device(&state, &key);
                current = Some((key, stream));
                pinned = wanted_pinned;
                failing_since = None;
            }
            Err(e) => {
                // Keep trying; give up after a minute without any device.
                let since = *failing_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(60) {
                    let _ = state
                        .errors
                        .send(CaptureNotice::error(format!("{label}: {e}")));
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
        .args([
            "-b:a",
            &BITRATE_BPS.to_string(),
            "-vbr",
            "on",
            "-application",
            "voip",
        ])
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
        return Err(format!(
            "FFmpeg failed to encode {} ({status})",
            path.display()
        ));
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
                    let _ = s
                        .errors
                        .send(CaptureNotice::error(format!("{}: {error}", s.side.label())));
                }
            },
            None,
        )
        .map_err(|e| format!("could not open the {} stream: {e}", state.side.label()))
}

fn open(
    state: &TrackState,
    device: &Device,
    config: SupportedStreamConfig,
) -> Result<Stream, String> {
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
    fn device_notice_classification_is_explicit() {
        assert_eq!(DeviceNoticeKind::DefaultSwitched.level(), NoticeLevel::Info);
        assert_eq!(DeviceNoticeKind::PinnedReturned.level(), NoticeLevel::Info);
        assert_eq!(DeviceNoticeKind::PinnedMissing.level(), NoticeLevel::Error);
        assert_eq!(
            CaptureNotice::error("fatal".into()).level,
            NoticeLevel::Error
        );
        let legacy: Session = serde_json::from_str(r#"{"status":"completed"}"#).unwrap();
        assert!(legacy.device_changes.is_empty());
    }

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
