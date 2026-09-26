use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, Stream, SupportedStreamConfig};
use serde_json::{Value, json};

const QUEUE_PACKETS: usize = 64;

#[derive(Clone)]
struct TrackSummary {
    file: &'static str,
    device: String,
    input_sample_rate: u32,
    channels: u16,
    bitrate_bps: u32,
    samples: u64,
    dropped_packets: u64,
    xruns: u64,
}

impl TrackSummary {
    fn duration_seconds(&self) -> f64 {
        self.samples as f64 / (self.input_sample_rate as f64 * self.channels as f64)
    }

    fn as_json(&self) -> Value {
        json!({
            "file": self.file,
            "device": self.device,
            "input_sample_rate": self.input_sample_rate,
            "sample_rate": 48000,
            "channels": self.channels,
            "container": "ogg",
            "encoding": "opus",
            "bitrate_bps": self.bitrate_bps,
            "samples": self.samples,
            "duration_seconds": self.duration_seconds(),
            "dropped_callback_packets": self.dropped_packets,
            "xruns": self.xruns,
        })
    }
}

struct Capture {
    label: &'static str,
    file: &'static str,
    device_name: String,
    stream: Stream,
    sender: SyncSender<Vec<i16>>,
    writer: JoinHandle<Result<u64, String>>,
    peak: Arc<AtomicU32>,
    dropped_packets: Arc<AtomicU64>,
    xruns: Arc<AtomicU64>,
    sample_rate: u32,
    channels: u16,
    bitrate_bps: u32,
}

impl Capture {
    fn finish(self) -> Result<TrackSummary, String> {
        let Capture {
            label,
            file,
            device_name,
            stream,
            sender,
            writer,
            peak: _,
            dropped_packets,
            xruns,
            sample_rate,
            channels,
            bitrate_bps,
        } = self;
        drop(stream);
        drop(sender);
        let samples = writer
            .join()
            .map_err(|_| format!("{label}: Opus encoder thread panicked"))??;
        Ok(TrackSummary {
            file,
            device: device_name,
            input_sample_rate: sample_rate,
            channels,
            bitrate_bps,
            samples,
            dropped_packets: dropped_packets.load(Ordering::Relaxed),
            xruns: xruns.load(Ordering::Relaxed),
        })
    }
}

fn spawn_writer(
    path: PathBuf,
    sample_rate: u32,
    channels: u16,
    bitrate_bps: u32,
    receiver: Receiver<Vec<i16>>,
) -> Result<JoinHandle<Result<u64, String>>, String> {
    thread::Builder::new()
        .name(format!(
            "opus-encoder-{}",
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("track")
        ))
        .spawn(move || {
            let sample_rate_arg = sample_rate.to_string();
            let channels_arg = channels.to_string();
            let bitrate_arg = bitrate_bps.to_string();
            let mut child = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-nostdin",
                    "-y",
                    "-f",
                    "s16le",
                    "-ar",
                    &sample_rate_arg,
                    "-ac",
                    &channels_arg,
                    "-i",
                    "pipe:0",
                    "-map_metadata",
                    "-1",
                    "-c:a",
                    "libopus",
                    "-b:a",
                    &bitrate_arg,
                    "-vbr",
                    "on",
                    "-application",
                    "voip",
                    "-f",
                    "ogg",
                ])
                .arg(&path)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| {
                    format!("could not start FFmpeg for {}: {error}", path.display())
                })?;
            let mut stdin = child.stdin.take().ok_or_else(|| {
                format!(
                    "FFmpeg did not provide an input pipe for {}",
                    path.display()
                )
            })?;
            let mut samples_written = 0u64;
            let mut write_error = None;
            while let Ok(packet) = receiver.recv() {
                let mut bytes = Vec::with_capacity(packet.len() * 2);
                for sample in packet.iter() {
                    bytes.extend_from_slice(&sample.to_le_bytes());
                }
                if let Err(error) = stdin.write_all(&bytes) {
                    write_error = Some(format!(
                        "could not stream audio to {}: {error}",
                        path.display()
                    ));
                    break;
                }
                samples_written += packet.len() as u64;
            }
            drop(stdin);
            let status = child
                .wait()
                .map_err(|error| format!("could not finalize {}: {error}", path.display()))?;
            if let Some(error) = write_error {
                return Err(error);
            }
            if !status.success() {
                return Err(format!(
                    "FFmpeg failed to encode {} (exit status: {status})",
                    path.display()
                ));
            }
            Ok(samples_written)
        })
        .map_err(|error| format!("could not start Opus encoder thread: {error}"))
}

fn check_ffmpeg() -> Result<(), String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map_err(|error| format!("FFmpeg is required and must be available on PATH: {error}"))?;
    let encoders = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() || !encoders.contains("libopus") {
        return Err(
            "FFmpeg with the libopus encoder is required; `ffmpeg -encoders` must list libopus"
                .to_owned(),
        );
    }
    Ok(())
}

fn pcm16(sample: f32) -> i16 {
    let scaled = (sample.clamp(-1.0, 1.0) * 32768.0).round();
    scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

fn build_stream<T>(
    label: &'static str,
    device: &Device,
    config: SupportedStreamConfig,
    sender: SyncSender<Vec<i16>>,
    peak: Arc<AtomicU32>,
    dropped_packets: Arc<AtomicU64>,
    xruns: Arc<AtomicU64>,
    fatal_error: Arc<AtomicBool>,
    error_sender: Sender<String>,
) -> Result<Stream, String>
where
    T: SizedSample + Copy,
    f32: FromSample<T>,
{
    let stream_config = config.config();
    let callback_error_sender = error_sender.clone();
    let callback_fatal_error = Arc::clone(&fatal_error);
    device
        .build_input_stream(
            stream_config,
            move |data: &[T], _| {
                let mut samples = Vec::with_capacity(data.len());
                let mut level = 0.0f32;
                for source in data.iter().copied() {
                    let sample = f32::from_sample(source);
                    level = level.max(sample.abs());
                    samples.push(pcm16(sample));
                }
                peak.fetch_max(level.to_bits(), Ordering::Relaxed);
                match sender.try_send(samples) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        dropped_packets.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        callback_fatal_error.store(true, Ordering::Relaxed);
                        let _ = callback_error_sender
                            .send(format!("{label}: Opus encoder stopped accepting audio"));
                    }
                }
            },
            move |error| {
                if error.kind() == cpal::ErrorKind::Xrun {
                    xruns.fetch_add(1, Ordering::Relaxed);
                } else {
                    fatal_error.store(true, Ordering::Relaxed);
                    let _ = error_sender.send(format!("{label} capture error: {error}"));
                }
            },
            None,
        )
        .map_err(|error| format!("could not open {label} stream: {error}"))
}

fn build_stream_for_format(
    label: &'static str,
    device: &Device,
    config: SupportedStreamConfig,
    sender: SyncSender<Vec<i16>>,
    peak: Arc<AtomicU32>,
    dropped_packets: Arc<AtomicU64>,
    xruns: Arc<AtomicU64>,
    fatal_error: Arc<AtomicBool>,
    error_sender: Sender<String>,
) -> Result<Stream, String> {
    macro_rules! stream {
        ($sample:ty) => {
            build_stream::<$sample>(
                label,
                device,
                config,
                sender,
                peak,
                dropped_packets,
                xruns,
                fatal_error,
                error_sender,
            )
        };
    }
    match config.sample_format() {
        SampleFormat::F32 => stream!(f32),
        SampleFormat::F64 => stream!(f64),
        SampleFormat::I8 => stream!(i8),
        SampleFormat::I16 => stream!(i16),
        SampleFormat::I32 => stream!(i32),
        SampleFormat::I64 => stream!(i64),
        SampleFormat::U8 => stream!(u8),
        SampleFormat::U16 => stream!(u16),
        SampleFormat::U32 => stream!(u32),
        SampleFormat::U64 => stream!(u64),
        other => Err(format!("unsupported {label} sample format: {other:?}")),
    }
}

fn start_capture(
    label: &'static str,
    file: &'static str,
    device: &Device,
    config: SupportedStreamConfig,
    path: &Path,
    fatal_error: Arc<AtomicBool>,
    error_sender: Sender<String>,
) -> Result<Capture, String> {
    let sample_rate = config.sample_rate();
    let channels = config.channels();
    let device_name = device
        .description()
        .map(|description| description.name().to_owned())
        .unwrap_or_else(|_| label.to_owned());
    let bitrate_bps = u32::from(channels) * 32_000;
    let (sender, receiver) = mpsc::sync_channel(QUEUE_PACKETS);
    let writer = spawn_writer(
        path.to_path_buf(),
        sample_rate,
        channels,
        bitrate_bps,
        receiver,
    )?;
    let peak = Arc::new(AtomicU32::new(0.0f32.to_bits()));
    let dropped_packets = Arc::new(AtomicU64::new(0));
    let xruns = Arc::new(AtomicU64::new(0));
    let stream = build_stream_for_format(
        label,
        device,
        config,
        sender.clone(),
        Arc::clone(&peak),
        Arc::clone(&dropped_packets),
        Arc::clone(&xruns),
        fatal_error,
        error_sender,
    )?;
    Ok(Capture {
        label,
        file,
        device_name,
        stream,
        sender,
        writer,
        peak,
        dropped_packets,
        xruns,
        sample_rate,
        channels,
        bitrate_bps,
    })
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn output_directory() -> PathBuf {
    PathBuf::from("recordings").join(format!("recording-{}", unix_millis()))
}

fn session_json(
    status: &str,
    started_at_unix_ms: u128,
    stopped_at_unix_ms: Option<u128>,
    mic: Option<&TrackSummary>,
    system: Option<&TrackSummary>,
    errors: &[String],
) -> Value {
    json!({
        "status": status,
        "started_at_unix_ms": started_at_unix_ms,
        "stopped_at_unix_ms": stopped_at_unix_ms,
        "tracks": {
            "mic": mic.map(TrackSummary::as_json),
            "system": system.map(TrackSummary::as_json),
        },
        "errors": errors,
    })
}

fn write_session(output_dir: &Path, value: &Value) -> Result<(), String> {
    let path = output_dir.join("session.json");
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("could not serialize {}: {error}", path.display()))?;
    fs::write(&path, bytes).map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn drain_errors(receiver: &Receiver<String>, errors: &mut Vec<String>) {
    while let Ok(error) = receiver.try_recv() {
        eprintln!("\n{error}");
        errors.push(error);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let output_dir = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(output_directory);
    if args.next().is_some() {
        return Err("usage: omarchy-meeting-recorder-windows [output-directory]".to_owned());
    }
    if ["mic.ogg", "system.ogg", "session.json"]
        .iter()
        .any(|file| output_dir.join(file).exists())
    {
        return Err(format!(
            "recording files already exist in {}",
            output_dir.display()
        ));
    }
    fs::create_dir_all(&output_dir)
        .map_err(|error| format!("could not create {}: {error}", output_dir.display()))?;
    check_ffmpeg()?;

    let host = cpal::default_host();
    let microphone = host
        .default_input_device()
        .ok_or("no default microphone found")?;
    let system_output = host
        .default_output_device()
        .ok_or("no default output device found")?;
    let mic_config = microphone
        .default_input_config()
        .map_err(|error| format!("could not read microphone format: {error}"))?;
    // On WASAPI, CPAL supports building an input stream on the render endpoint;
    // this enables shared-mode system loopback capture.
    let system_config = system_output
        .default_output_config()
        .map_err(|error| format!("could not read system-output format: {error}"))?;

    let (error_sender, error_receiver) = mpsc::channel();
    let fatal_error = Arc::new(AtomicBool::new(false));
    let mic = start_capture(
        "microphone",
        "mic.ogg",
        &microphone,
        mic_config,
        &output_dir.join("mic.ogg"),
        Arc::clone(&fatal_error),
        error_sender.clone(),
    )?;
    let system = start_capture(
        "system loopback",
        "system.ogg",
        &system_output,
        system_config,
        &output_dir.join("system.ogg"),
        Arc::clone(&fatal_error),
        error_sender,
    )?;

    let stop = Arc::new(AtomicBool::new(false));
    let ctrl_c_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || ctrl_c_stop.store(true, Ordering::SeqCst))
        .map_err(|error| format!("could not install Ctrl+C handler: {error}"))?;

    if let Err(error) = mic.stream.play() {
        let _ = mic.finish();
        let _ = system.finish();
        return Err(format!("could not start microphone: {error}"));
    }
    if let Err(error) = system.stream.play() {
        let _ = mic.finish();
        let _ = system.finish();
        return Err(format!("could not start system loopback: {error}"));
    }

    let started_at_unix_ms = unix_millis();
    write_session(
        &output_dir,
        &session_json("recording", started_at_unix_ms, None, None, None, &[]),
    )?;

    println!("Recording separate tracks to {}", output_dir.display());
    println!("Microphone: {}", mic.device_name);
    println!("System output loopback: {}", system.device_name);
    println!("Encoding: Ogg Opus; 32 kbps per channel, speech optimized.");
    println!("Press Enter or Ctrl+C to stop.");

    let enter_stop = Arc::clone(&stop);
    thread::spawn(move || {
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
        enter_stop.store(true, Ordering::SeqCst);
    });

    let mut errors = Vec::new();
    while !stop.load(Ordering::SeqCst) && !fatal_error.load(Ordering::Relaxed) {
        drain_errors(&error_receiver, &mut errors);
        let mic_level = f32::from_bits(mic.peak.swap(0, Ordering::Relaxed)).clamp(0.0, 1.0);
        let system_level = f32::from_bits(system.peak.swap(0, Ordering::Relaxed)).clamp(0.0, 1.0);
        eprint!(
            "\rMic {:>3.0}% ({} xruns) | System {:>3.0}% ({} xruns)",
            mic_level * 100.0,
            mic.xruns.load(Ordering::Relaxed),
            system_level * 100.0,
            system.xruns.load(Ordering::Relaxed),
        );
        thread::sleep(Duration::from_millis(100));
    }
    drain_errors(&error_receiver, &mut errors);
    eprintln!("\nStopping and finalizing Opus files...");

    let stopped_at_unix_ms = unix_millis();
    let mic_result = mic.finish();
    let system_result = system.finish();
    if let Err(error) = &mic_result {
        errors.push(error.clone());
    }
    if let Err(error) = &system_result {
        errors.push(error.clone());
    }
    let mic_summary = mic_result.ok();
    let system_summary = system_result.ok();
    let failed =
        fatal_error.load(Ordering::Relaxed) || mic_summary.is_none() || system_summary.is_none();
    write_session(
        &output_dir,
        &session_json(
            if failed { "failed" } else { "completed" },
            started_at_unix_ms,
            Some(stopped_at_unix_ms),
            mic_summary.as_ref(),
            system_summary.as_ref(),
            &errors,
        ),
    )?;

    for summary in [mic_summary.as_ref(), system_summary.as_ref()]
        .into_iter()
        .flatten()
    {
        println!(
            "Saved {} (input {} Hz, {} channels, {} kbps, {:.2} s, {} dropped packets, {} xruns)",
            summary.file,
            summary.input_sample_rate,
            summary.channels,
            summary.bitrate_bps / 1000,
            summary.duration_seconds(),
            summary.dropped_packets,
            summary.xruns,
        );
    }
    println!("Saved session.json");
    if failed {
        return Err("capture ended with errors; see session.json".to_owned());
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
