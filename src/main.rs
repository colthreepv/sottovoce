#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Sottovoce development CLI: records your microphone and the computer's
//! audio as separate tracks, finds the speakers on each side with NVIDIA
//! Nemotron 3 Diarization, transcribes them with ElevenLabs and shows the
//! conversation.

use sottovoce_engine::{
    capture, config, devices, diarize, elevenlabs, ffmpeg, meetings, pipeline, transcript, types,
};

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use types::{Abort, Event};

use sottovoce_engine::log_event;

fn main() {
    std::panic::set_hook(Box::new(|panic| log_event(&format!("PANIC: {panic}"))));
    log_event("Application started");
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("{}", usage());
        std::process::exit(2);
    }
    attach_console();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", usage());
        return;
    }
    let result = validate_args(&args).and_then(|()| match args[0].as_str() {
        "record" => cli_record(&args[1..]),
        "process" => cli_process(&args[1..]),
        "diarize" => cli_diarize(&args[1..]),
        "stt" => cli_stt(&args[1..]),
        "devices" => cli_devices(),
        "selftest" => cli_selftest(),
        _ => Err(usage()),
    });
    if let Err(e) = result {
        log_event(&format!("Error: {e}"));
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn validate_args(args: &[String]) -> Result<(), String> {
    let (minimum, maximum, allowed): (usize, usize, &[&str]) = match args[0].as_str() {
        "record" => (0, 1, &[]),
        "process" => (1, 1, &[]),
        "diarize" => (1, 1, &["--speakers", "-s"]),
        "stt" => (1, 1, &["--language", "-l"]),
        "devices" | "selftest" => (0, 0, &[]),
        _ => return Err(usage()),
    };
    let mut positional = 0;
    let mut iter = args[1..].iter();
    while let Some(arg) = iter.next() {
        if allowed.contains(&arg.as_str()) {
            let value = iter
                .next()
                .filter(|v| !v.starts_with('-'))
                .ok_or_else(|| format!("{arg} needs a value"))?;
            if matches!(arg.as_str(), "--speakers" | "-s")
                && value.parse::<usize>().ok().filter(|n| *n > 0).is_none()
            {
                return Err("--speakers needs a positive integer".into());
            }
        } else if arg.starts_with('-') {
            return Err(format!("Unknown option: {arg}"));
        } else {
            positional += 1;
        }
    }
    if positional < minimum || positional > maximum {
        return Err(usage());
    }
    Ok(())
}

fn usage() -> String {
    [
        "Usage: sottovoce-dev-cli record [folder]       record until Enter",
        "       sottovoce-dev-cli process <folder>      transcribe a recorded meeting",
        "       sottovoce-dev-cli diarize <audio> [--speakers N]",
        "       sottovoce-dev-cli stt <audio> [--language xx]",
        "       sottovoce-dev-cli devices               list audio devices",
        "       sottovoce-dev-cli selftest              record while playing a tone",
    ]
    .join("\n")
}

/// A release build is a GUI program; show CLI output in the calling console.
fn attach_console() {
    #[cfg(windows)]
    unsafe {
        unsafe extern "system" {
            fn AttachConsole(process_id: u32) -> i32;
        }
        AttachConsole(u32::MAX);
    }
}

/// Prints events on stderr until the sender side is dropped.
fn print_events() -> (types::Events, std::thread::JoinHandle<()>) {
    let (sender, receiver) = std::sync::mpsc::channel::<Event>();
    let handle = std::thread::spawn(move || {
        let mut last = -1i64;
        while let Ok(event) = receiver.recv() {
            match event {
                Event::Stage(stage) => {
                    eprintln!("{stage}");
                    last = -1;
                }
                Event::Progress(p) => {
                    let pct = (p * 100.0) as i64;
                    if pct / 10 != last / 10 {
                        eprintln!("  {pct}%");
                        last = pct;
                    }
                }
                Event::Log(line) => eprintln!("  {line}"),
            }
        }
    });
    (sender, handle)
}

fn cli_record(args: &[String]) -> Result<(), String> {
    let config = config::Config::load();
    let dir = match args.first() {
        Some(dir) => PathBuf::from(dir),
        None => meetings::new_dir(&config.meetings_dir(), capture::unix_ms()),
    };
    let mut recorder = capture::Recorder::start(
        &dir,
        devices::DeviceChoice::from(config.mic_device()),
        devices::DeviceChoice::from(config.output_device()),
    )?;
    println!("Recording to {}", dir.display());
    println!("Microphone: {}", recorder.mic_device());
    println!("Computer audio: {}", recorder.computer_device());
    println!("Press Enter or Ctrl+C to stop.");
    let stop = Arc::new(AtomicBool::new(false));
    let on_ctrl_c = Arc::clone(&stop);
    let _ = ctrlc::set_handler(move || on_ctrl_c.store(true, Ordering::SeqCst));
    let on_enter = Arc::clone(&stop);
    std::thread::spawn(move || {
        let _ = std::io::stdin().read_line(&mut String::new());
        on_enter.store(true, Ordering::SeqCst);
    });
    while !stop.load(Ordering::SeqCst) {
        let (_, fatal) = recorder.poll_errors();
        if fatal {
            break;
        }
        let (mic, computer) = recorder.levels();
        eprint!(
            "\r{:>6.1}s  mic {:>3.0}%  computer {:>3.0}%   ",
            recorder.elapsed().as_secs_f64(),
            mic * 100.0,
            computer * 100.0
        );
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    eprintln!();
    let session = recorder.stop()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&session).unwrap_or_default()
    );
    if session.status != "completed" {
        return Err("the recording ended with errors".into());
    }
    Ok(())
}

fn cli_process(args: &[String]) -> Result<(), String> {
    let dir = PathBuf::from(args.first().ok_or_else(usage)?);
    let options = pipeline::Options::from_config(&config::Config::load())?;
    let (events, printer) = print_events();
    let result = pipeline::process(&dir, &options, &events, &Abort::default());
    drop(events);
    let _ = printer.join();
    let meeting = result?;
    println!("{}", transcript::to_markdown(&meeting));
    Ok(())
}

fn cli_diarize(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut speakers = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--speakers" | "-s" => speakers = iter.next().and_then(|n| n.parse().ok()),
            other => path = Some(PathBuf::from(other)),
        }
    }
    let path = path.ok_or_else(usage)?;
    let samples = ffmpeg::decode_mono_16k(&path)?;
    let (events, printer) = print_events();
    let started = std::time::Instant::now();
    let result = diarize::turns(&samples, speakers, &events, &Abort::default());
    drop(events);
    let _ = printer.join();
    let turns = result?;
    eprintln!(
        "{} turns in {:.1}s for {:.0}s of audio",
        turns.len(),
        started.elapsed().as_secs_f64(),
        samples.len() as f64 / 16_000.0
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&turns).unwrap_or_default()
    );
    Ok(())
}

fn cli_stt(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut language = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--language" | "-l" => language = iter.next().cloned(),
            other => path = Some(PathBuf::from(other)),
        }
    }
    let path = path.ok_or_else(usage)?;
    let mut options = pipeline::Options::from_config(&config::Config::load())?;
    if language.is_some() {
        options.stt.language = language;
    }
    let result = elevenlabs::transcribe_file(&path, &options.stt, None, &Abort::default())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&result.words).unwrap_or_default()
    );
    eprintln!("language: {:?}", result.language_code);
    Ok(())
}

fn cli_devices() -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let name = |d: &cpal::Device| {
        d.description()
            .map(|d| d.name().to_owned())
            .unwrap_or_else(|e| format!("? ({e})"))
    };
    let default_in = host.default_input_device().map(|d| name(&d));
    let default_out = host.default_output_device().map(|d| name(&d));
    println!("default input:  {}", default_in.unwrap_or_default());
    println!("default output: {}", default_out.unwrap_or_default());
    for device in host.output_devices().map_err(|e| e.to_string())? {
        let config = device.default_output_config().map(|c| format!("{c:?}"));
        println!(
            "output: {}  id={}  {}",
            name(&device),
            device.id().map(|id| id.to_string()).unwrap_or_default(),
            config.unwrap_or_else(|e| e.to_string())
        );
    }
    for device in host.input_devices().map_err(|e| e.to_string())? {
        let config = device.default_input_config().map(|c| format!("{c:?}"));
        println!(
            "input:  {}  id={}  {}",
            name(&device),
            device.id().map(|id| id.to_string()).unwrap_or_default(),
            config.unwrap_or_else(|e| e.to_string())
        );
    }
    Ok(())
}

/// Records for four seconds while a quiet tone plays on the default output,
/// then reports the peak of both tracks: the computer track must hear the tone.
fn cli_selftest() -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let dir =
        std::env::temp_dir().join(format!("sottovoce-selftest-{}", capture::unix_ms()));
    let recorder = capture::Recorder::start(
        &dir,
        devices::DeviceChoice::FollowDefault,
        devices::DeviceChoice::FollowDefault,
    )?;
    println!("Recording to {}", dir.display());
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no default output device")?;
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let rate = config.sample_rate() as f32;
    let channels = usize::from(config.channels());
    let mut phase = 0.0f32;
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let stream = device
        .build_output_stream(
            config.config(),
            move |data: &mut [f32], _| {
                for frame in data.chunks_mut(channels) {
                    phase = (phase + 660.0 / rate) % 1.0;
                    let v = (phase * std::f32::consts::TAU).sin() * 0.2;
                    frame.iter_mut().for_each(|s| *s = v);
                }
            },
            |e| eprintln!("playback error: {e}"),
            None,
        )
        .map_err(|e| format!("could not play a tone (needs an f32 output): {e}"))?;
    stream.play().map_err(|e| e.to_string())?;
    std::thread::sleep(std::time::Duration::from_millis(2000));
    drop(stream);
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let session = recorder.stop()?;
    for side in [types::Side::Mic, types::Side::Computer] {
        let samples = ffmpeg::decode_mono_16k(&dir.join(side.file_name()))?;
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        println!(
            "{:<8} {:>5.1}s  peak {:>5.1} dBFS",
            side.label(),
            samples.len() as f32 / 16_000.0,
            20.0 * peak.max(1e-6).log10()
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&session).unwrap_or_default()
    );
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn rejects_unknown_flags_and_excess_paths() {
        for input in [
            vec!["record", "--bogus"],
            vec!["devices", "extra"],
            vec!["process", "one", "two"],
            vec!["diarize", "a", "--speakers"],
            vec!["diarize", "a", "-s", "0"],
            vec!["stt", "a", "--bogus"],
        ] {
            assert!(validate_args(&args(&input)).is_err(), "{input:?}");
        }
        assert!(validate_args(&args(&["record"])).is_ok());
        assert!(validate_args(&args(&["diarize", "a", "-s", "2"])).is_ok());
    }
}
