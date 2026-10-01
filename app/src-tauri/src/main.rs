#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use serde::Serialize;
use sottovoce_engine::{
    config::Config,
    core::{Core, Devices, Event, MeetingEntry, RecordingState},
    types::Meeting,
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use tauri::{Emitter, Manager, State};

#[derive(Clone, Serialize)]
struct Snapshot {
    config: Config,
    devices: Devices,
    state: RecordingState,
    elapsed_ms: u64,
    recording_meeting: Option<PathBuf>,
    closing: bool,
    meetings: Vec<MeetingEntry>,
}
struct Engine {
    core: Core,
    latest: Mutex<Snapshot>,
    closing: AtomicBool,
    complete: AtomicBool,
}
fn lock_core(state: &Engine) -> Result<&Core, String> {
    Ok(&state.core)
}
#[tauri::command]
fn start_recording(state: State<Engine>) -> Result<(), String> {
    lock_core(&state)?.start_recording()
}
#[tauri::command]
fn stop_recording(state: State<Engine>) -> Result<(), String> {
    lock_core(&state)?.stop_recording()
}
#[tauri::command]
fn transcribe(meeting: PathBuf, state: State<Engine>) -> Result<(), String> {
    lock_core(&state)?.transcribe(meeting)
}
#[tauri::command]
fn cancel_job(meeting: PathBuf, state: State<Engine>) -> Result<(), String> {
    lock_core(&state)?.cancel_job(meeting)
}
#[tauri::command]
fn list_meetings(state: State<Engine>) -> Result<Vec<MeetingEntry>, String> {
    Ok(lock_core(&state)?.list_meetings())
}
#[tauri::command]
async fn rename_meeting(
    meeting: PathBuf,
    title: String,
    app: tauri::AppHandle,
) -> Result<PathBuf, String> {
    tauri::async_runtime::spawn_blocking(move || {
        lock_core(&app.state::<Engine>())?.rename_meeting(meeting, title)
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
fn delete_audio(meeting: PathBuf, state: State<Engine>) -> Result<(), String> {
    lock_core(&state)?.delete_audio(meeting)
}
#[tauri::command]
fn archive_meeting(meeting: PathBuf, state: State<Engine>) -> Result<(), String> {
    lock_core(&state)?.archive_meeting(meeting)
}
#[tauri::command]
fn set_mic_device(id: Option<String>, state: State<Engine>) -> Result<(), String> {
    lock_core(&state)?.set_mic_device(id)
}
#[tauri::command]
fn set_output_device(id: Option<String>, state: State<Engine>) -> Result<(), String> {
    lock_core(&state)?.set_output_device(id)
}
#[tauri::command]
fn list_devices(state: State<Engine>) -> Result<(), String> {
    lock_core(&state)?.list_devices()
}
#[tauri::command]
fn get_config(state: State<Engine>) -> Result<Config, String> {
    Ok(lock_core(&state)?.get_config())
}
#[tauri::command]
fn update_config(config: Config, state: State<Engine>) -> Result<(), String> {
    lock_core(&state)?.update_config(config)
}
#[tauri::command]
fn get_snapshot(state: State<Engine>) -> Result<Snapshot, String> {
    let core = lock_core(&state)?;
    let mut snapshot = state
        .latest
        .lock()
        .map_err(|_| "Snapshot lock failed")?
        .clone();
    snapshot.config = core.get_config();
    snapshot.meetings = core.list_meetings();
    snapshot.state = core.recording_state();
    snapshot.closing = state.closing.load(Ordering::SeqCst);
    Ok(snapshot)
}
// Validate at the file boundary and grant only the selected audio files to the
// asset protocol. This also works for manually changed meetings directories.
fn meeting_dir(core: &Core, dir: &Path) -> Result<PathBuf, String> {
    let requested = dir.canonicalize().map_err(|e| e.to_string())?;
    if !core
        .list_meetings()
        .iter()
        .any(|e| e.meeting.dir.canonicalize().ok().as_ref() == Some(&requested))
    {
        return Err("Meeting is outside the library".into());
    }
    Ok(requested)
}
#[derive(Serialize)]
struct MeetingView {
    meeting: Meeting,
    mic: Option<PathBuf>,
    system: Option<PathBuf>,
}
#[tauri::command]
fn get_meeting(
    meeting: PathBuf,
    app: tauri::AppHandle,
    state: State<Engine>,
) -> Result<MeetingView, String> {
    let dir = meeting_dir(lock_core(&state)?, &meeting)?;
    let metadata = sottovoce_engine::meetings::load(&dir).ok_or("Cannot read meeting metadata")?;
    let track = |name: &str| -> Result<Option<PathBuf>, String> {
        let path = dir.join(name);
        if !path.is_file() {
            return Ok(None);
        }
        let path = path.canonicalize().map_err(|e| e.to_string())?;
        if path.parent() != Some(dir.as_path()) {
            return Err("Audio file points outside the meeting".into());
        }
        app.asset_protocol_scope()
            .allow_file(&path)
            .map_err(|e| e.to_string())?;
        Ok(Some(path))
    };
    Ok(MeetingView {
        meeting: metadata,
        mic: track("mic.ogg")?,
        system: track("computer.ogg")?,
    })
}
fn begin_shutdown(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<Engine>();
    if state.closing.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let _ = app.emit("closing", ());
    let (tx, rx) = mpsc::channel();
    app.manage(CloseWatchdog(Mutex::new(Some(tx))));
    // Install the watchdog before asking Core to stop: shutdown may complete
    // immediately when idle. Completion wakes the wait without polling.
    let handle = app.clone();
    std::thread::spawn(move || {
        if rx.recv_timeout(Duration::from_secs(30)).is_err()
            && !handle.state::<Engine>().complete.load(Ordering::SeqCst)
        {
            sottovoce_engine::log_event("Shutdown exceeded 30 seconds; exiting");
            tauri_plugin_single_instance::destroy(&handle);
            std::process::exit(0);
        }
    });
    lock_core(&state)?.shutdown()?;
    Ok(())
}

struct CloseWatchdog(Mutex<Option<mpsc::Sender<()>>>);
#[tauri::command]
fn shutdown(app: tauri::AppHandle) -> Result<(), String> {
    begin_shutdown(&app)
}
fn main() {
    std::panic::set_hook(Box::new(|p| {
        sottovoce_engine::log_event(&format!("PANIC: {p}"))
    }));
    tauri::Builder::default()
        // Registered first: a second launch must exit before creating a Core.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .setup(|app| {
            let (core, receiver) = Core::new()?;
            let snapshot = Snapshot {
                config: core.get_config(),
                devices: Devices::default(),
                state: core.recording_state(),
                elapsed_ms: 0,
                recording_meeting: None,
                closing: false,
                meetings: vec![],
            };
            app.manage(Engine {
                core,
                latest: Mutex::new(snapshot),
                closing: AtomicBool::new(false),
                complete: AtomicBool::new(false),
            });
            let handle = app.handle().clone();
            std::thread::Builder::new()
                .name("sottovoce-webview-events".into())
                .spawn(move || {
                    while let Ok(event) = receiver.recv() {
                        let state = handle.state::<Engine>();
                        if let Ok(mut latest) = state.latest.lock() {
                            match &event {
                                Event::DevicesChanged { devices } => {
                                    latest.devices = devices.clone()
                                }
                                Event::RecordingStateChanged {
                                    state,
                                    elapsed_ms,
                                    meeting,
                                } => {
                                    latest.state = *state;
                                    latest.elapsed_ms = *elapsed_ms;
                                    latest.recording_meeting = meeting.clone();
                                }
                                Event::ConfigChanged { config } => latest.config = config.clone(),
                                _ => (),
                            }
                        }
                        let done = matches!(event, Event::ShutdownComplete);
                        let _ = handle.emit("core-event", &event);
                        if done {
                            state.complete.store(true, Ordering::SeqCst);
                            if let Some(watchdog) = handle.try_state::<CloseWatchdog>() {
                                if let Some(tx) = watchdog.0.lock().unwrap().take() {
                                    let _ = tx.send(());
                                }
                            }
                            handle.exit(0);
                            break;
                        }
                    }
                })?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_recording,
            stop_recording,
            transcribe,
            cancel_job,
            list_meetings,
            rename_meeting,
            delete_audio,
            archive_meeting,
            set_mic_device,
            set_output_device,
            list_devices,
            get_config,
            update_config,
            shutdown,
            get_snapshot,
            get_meeting
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Err(error) = begin_shutdown(window.app_handle()) {
                    let _ = window.emit("core-event", Event::Error { message: error });
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("Could not initialize Sottovoce")
        .run(|_, _| {});
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    struct SilentCapture;
    impl sottovoce_engine::core::Capture for SilentCapture {
        fn monitor(&mut self, _: &Config) -> Result<(), String> {
            Ok(())
        }
        fn stop_monitor(&mut self) {}
        fn start(&mut self, _: &Path, _: &Config) -> Result<(), String> {
            Err("No hardware in bridge tests".into())
        }
        fn stop(&mut self) -> Result<sottovoce_engine::capture::Session, String> {
            Err("Not recording".into())
        }
        fn levels(&mut self) -> (f32, f32) {
            (0.0, 0.0)
        }
    }
    struct NoTranscription;
    impl sottovoce_engine::core::Processor for NoTranscription {
        fn process(
            &self,
            _: &Path,
            _: &Config,
            _: &sottovoce_engine::types::Events,
            _: &sottovoce_engine::types::Abort,
        ) -> Result<Meeting, String> {
            Err("No API calls in tests".into())
        }
    }
    #[test]
    fn meeting_boundary_rejects_existing_foreign_directories() {
        let root = std::env::temp_dir().join(format!(
            "sottovoce-bridge-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let library = root.join("meetings");
        let member = library.join("sample");
        let foreign = root.join("foreign");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::create_dir_all(&foreign).unwrap();
        sottovoce_engine::meetings::save(&member, &Meeting::default()).unwrap();
        let config = root.join("config.toml");
        Config {
            meetings_dir: Some(library),
            ..Default::default()
        }
        .save_at(&config)
        .unwrap();
        let (core, receiver) = Core::with_backends(
            config,
            || Box::new(SilentCapture),
            std::sync::Arc::new(NoTranscription),
        )
        .unwrap();
        assert_eq!(
            meeting_dir(&core, &member).unwrap(),
            member.canonicalize().unwrap()
        );
        assert!(meeting_dir(&core, &foreign).is_err());
        assert!(meeting_dir(&core, &root.join("missing")).is_err());
        core.shutdown().unwrap();
        while !matches!(
            receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::ShutdownComplete
        ) {}
        drop(core);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn serde_contract_keeps_explicit_nulls_and_event_tags() {
        let snapshot = Snapshot {
            config: Config::default(),
            devices: Devices::default(),
            state: RecordingState::Ready,
            elapsed_ms: 0,
            recording_meeting: None,
            closing: false,
            meetings: vec![],
        };
        let json = serde_json::to_value(snapshot).unwrap();
        assert_eq!(json["state"], "ready");
        assert!(json["recording_meeting"].is_null());
        assert!(json["config"].get("archive_dir").unwrap().is_null());
        let event = serde_json::to_value(Event::Levels {
            mic: 0.0,
            system: 0.25,
        })
        .unwrap();
        assert_eq!(event["type"], "levels");
        assert_eq!(event["system"], 0.25);
    }
}
