//! Shared Sottovoce engine for the legacy egui and Tauri frontends.
pub mod app_audio;
pub mod capture;
pub mod config;
pub mod core;
pub mod devices;
pub mod diarize;
pub mod elevenlabs;
pub mod ffmpeg;
pub mod meetings;
pub mod library;
pub mod nemotron;
pub mod paths;
pub mod pipeline;
pub mod player;
pub mod transcript;
pub mod types;

pub const APP_NAME: &str = "Meeting Recorder";

pub fn log_event(message: &str) {
    use std::io::Write;
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let Ok(_guard) = LOCK.lock() else { return };
    let dir = paths::data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.log", chrono::Local::now().format("%Y-%m-%d")));
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(
            file,
            "{} [pid {}] {message}",
            chrono::Local::now().to_rfc3339(),
            std::process::id()
        );
    }
}

/// CPAL defaults to STA; WASAPI workers use MTA to avoid GUI message-pump dependencies.
pub fn audio_thread_init() {
    #[cfg(windows)]
    unsafe {
        #[link(name = "ole32")]
        unsafe extern "system" {
            fn CoInitializeEx(reserved: *mut std::ffi::c_void, mode: u32) -> i32;
        }
        let result = CoInitializeEx(std::ptr::null_mut(), 0);
        if result < 0 {
            log_event(&format!("Audio COM initialization: {result:#x}"));
        }
    }
}

