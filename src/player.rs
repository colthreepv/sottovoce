//! Non-blocking playback for a meeting's microphone and computer tracks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

const RATE: u32 = 48_000;

struct Shared {
    samples: Mutex<Option<Vec<f32>>>,
    error: Mutex<Option<String>>,
    position: AtomicU64,
    playing: AtomicBool,
    loading: AtomicBool,
    duration_ms: AtomicU64,
}

/// Plays the two tracks of a meeting, mixing them into stereo.
pub struct Player {
    shared: Arc<Shared>,
    _quit: std::sync::mpsc::Sender<()>,
}

impl Player {
    /// Starts loading both tracks. Decoding never blocks the caller.
    pub fn new(dir: &Path) -> Player {
        let shared = Arc::new(Shared {
            samples: Mutex::new(None),
            error: Mutex::new(None),
            position: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            loading: AtomicBool::new(true),
            duration_ms: AtomicU64::new(0),
        });
        let path = dir.to_path_buf();
        let decode_shared = Arc::clone(&shared);
        let _ = thread::Builder::new()
            .name("meeting-playback-decode".into())
            .spawn(move || decode_tracks(path, decode_shared));

        let (quit, rx) = std::sync::mpsc::channel();
        let output_shared = Arc::clone(&shared);
        thread::spawn(move || {
            crate::audio_thread_init();
            // WASAPI objects never originate in winit's STA apartment.
            match open_output(Arc::clone(&output_shared)) {
                Ok(stream) => {
                    let _ = rx.recv();
                    drop(stream);
                }
                Err(error) => {
                    crate::log_event(&error);
                    if let Ok(mut slot) = output_shared.error.lock() {
                        *slot = Some(error);
                    }
                }
            }
        });
        Player {
            shared,
            _quit: quit,
        }
    }

    /// Seeks to a time in milliseconds and starts playback.
    pub fn play_from(&self, ms: u64) {
        let duration = self.duration_ms();
        let position = if self.is_loading() {
            ms
        } else {
            ms.min(duration)
        };
        self.shared.position.store(position, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    /// Pauses playback at the current position.
    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    /// Toggles between playing and paused.
    pub fn toggle(&self) {
        if self.is_playing() {
            self.pause();
        } else {
            let pos = self.position_ms();
            if pos >= self.duration_ms() {
                self.play_from(0);
            } else {
                self.shared.playing.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn position_ms(&self) -> u64 {
        self.shared.position.load(Ordering::Relaxed)
    }

    pub fn duration_ms(&self) -> u64 {
        self.shared.duration_ms.load(Ordering::Relaxed)
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn is_loading(&self) -> bool {
        self.shared.loading.load(Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.shared
            .error
            .lock()
            .ok()
            .and_then(|error| error.clone())
    }
}

fn decode_tracks(dir: PathBuf, shared: Arc<Shared>) {
    let mic = decode_track(&dir.join("mic.ogg"));
    let computer = decode_track(&dir.join("computer.ogg"));
    let (mic, computer) = match (mic, computer) {
        (Ok(mic), Ok(computer)) => (mic, computer),
        (Err(error), _) | (_, Err(error)) => {
            if let Ok(mut slot) = shared.error.lock() {
                *slot = Some(error);
            }
            shared.loading.store(false, Ordering::Relaxed);
            return;
        }
    };
    let frames = mic.len().max(computer.len());
    let mut stereo = Vec::with_capacity(frames.saturating_mul(2));
    for i in 0..frames {
        let mixed =
            soft_clip(mic.get(i).copied().unwrap_or(0.0) + computer.get(i).copied().unwrap_or(0.0));
        stereo.extend_from_slice(&[mixed, mixed]);
    }
    shared
        .duration_ms
        .store((frames as u64 * 1000) / u64::from(RATE), Ordering::Relaxed);
    if let Ok(mut buffer) = shared.samples.lock() {
        *buffer = Some(stereo);
    }
    let position = shared
        .position
        .load(Ordering::Relaxed)
        .min(shared.duration_ms.load(Ordering::Relaxed));
    shared.position.store(position, Ordering::Relaxed);
    shared.loading.store(false, Ordering::Relaxed);
}

fn decode_track(path: &Path) -> Result<Vec<f32>, String> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    crate::ffmpeg::decode(path, RATE, 1)
}

fn soft_clip(sample: f32) -> f32 {
    // Leave quiet speech unchanged and smoothly approach full scale above it.
    let magnitude = sample.abs();
    if magnitude <= 0.8 {
        sample
    } else {
        sample.signum() * (1.0 - 0.2 * (-(magnitude - 0.8) / 0.2).exp())
    }
}

fn open_output(shared: Arc<Shared>) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no default output device found")?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("could not read output format: {e}"))?;
    let config: StreamConfig = supported.clone().into();
    let rate = config.sample_rate;
    let channels = usize::from(config.channels.max(1));
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, config, shared, rate, channels),
        SampleFormat::F64 => build::<f64>(&device, config, shared, rate, channels),
        SampleFormat::I8 => build::<i8>(&device, config, shared, rate, channels),
        SampleFormat::I16 => build::<i16>(&device, config, shared, rate, channels),
        SampleFormat::I32 => build::<i32>(&device, config, shared, rate, channels),
        SampleFormat::U8 => build::<u8>(&device, config, shared, rate, channels),
        SampleFormat::U16 => build::<u16>(&device, config, shared, rate, channels),
        SampleFormat::U32 => build::<u32>(&device, config, shared, rate, channels),
        other => Err(format!("unsupported output sample format: {other:?}")),
    }?;
    stream
        .play()
        .map_err(|e| format!("could not start playback: {e}"))?;
    Ok(stream)
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    shared: Arc<Shared>,
    output_rate: u32,
    channels: usize,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let error_shared = Arc::clone(&shared);
    let callback_error = Arc::clone(&shared);
    device
        .build_output_stream(
            config,
            move |output: &mut [T], _| {
                let Ok(buffer) = shared.samples.try_lock() else {
                    for sample in output {
                        *sample = T::from_sample(0.0);
                    }
                    return;
                };
                let Some(samples) = buffer.as_ref() else {
                    for sample in output {
                        *sample = T::from_sample(0.0);
                    }
                    return;
                };
                let step = f64::from(RATE) / f64::from(output_rate.max(1));
                let mut frame =
                    shared.position.load(Ordering::Relaxed) as f64 * f64::from(RATE) / 1000.0;
                let advance = shared.playing.load(Ordering::Relaxed);
                for output_frame in output.chunks_mut(channels) {
                    let index = frame as usize;
                    let value = if advance && index + 1 < samples.len() / 2 {
                        let frac = (frame - index as f64) as f32;
                        let a = samples[index * 2];
                        let b = samples[(index + 1) * 2];
                        a + (b - a) * frac
                    } else {
                        0.0
                    };
                    for sample in output_frame {
                        *sample = T::from_sample(value);
                    }
                    if advance {
                        frame += step;
                    }
                }
                if advance {
                    let pos = (frame * 1000.0 / f64::from(RATE)) as u64;
                    let duration = error_shared.duration_ms.load(Ordering::Relaxed);
                    if pos >= duration {
                        error_shared.position.store(duration, Ordering::Relaxed);
                        error_shared.playing.store(false, Ordering::Relaxed);
                    } else {
                        error_shared.position.store(pos, Ordering::Relaxed);
                    }
                }
            },
            move |error| {
                if let Ok(mut slot) = callback_error.error.lock() {
                    *slot = Some(format!("Playback failed: {error}"));
                }
                callback_error.playing.store(false, Ordering::Relaxed);
            },
            Some(Duration::from_millis(100)),
        )
        .map_err(|e| format!("could not open output stream: {e}"))
}
