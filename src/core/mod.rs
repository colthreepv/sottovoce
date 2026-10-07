//! UI-independent command/event engine. Capture is owned by one persistent MTA
//! thread; the independent FIFO worker owns transcription. Commands are accepted
//! asynchronously: operation failures are reported through Event::Error.
use crate::{
    config::Config,
    devices::AudioDevice,
    meetings::Entry,
    types::{Abort, Meeting},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    Ready,
    Starting,
    Recording,
    Finalizing,
}
impl RecordingState {
    pub fn transition(self, next: Self) -> Result<Self, String> {
        use RecordingState::*;
        if matches!(
            (self, next),
            (Ready, Starting)
                | (Starting, Recording)
                | (Starting, Ready)
                | (Recording, Finalizing)
                | (Finalizing, Ready)
        ) {
            Ok(next)
        } else {
            Err(format!(
                "Invalid recording transition: {self:?} -> {next:?}"
            ))
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Cancelling,
    Done,
    Cancelled,
    Failed(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeetingEntry {
    pub meeting: Entry,
    pub job: Option<JobState>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Devices {
    pub inputs: Vec<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Levels {
        mic: f32,
        system: f32,
    },
    MonitoringChanged {
        active: bool,
    },
    Notice {
        message: String,
    },
    RecordingStateChanged {
        state: RecordingState,
        elapsed_ms: u64,
        meeting: Option<PathBuf>,
    },
    JobQueued {
        meeting: PathBuf,
    },
    JobProgress {
        meeting: PathBuf,
        stage: String,
        progress: f64,
    },
    JobDone {
        meeting: PathBuf,
    },
    JobFailed {
        meeting: PathBuf,
        error: String,
    },
    JobCancelled {
        meeting: PathBuf,
    },
    LibraryBusy {
        meeting: PathBuf,
        busy: bool,
    },
    LibraryDone {
        meeting: PathBuf,
        action: crate::library::Action,
        output: Option<PathBuf>,
    },
    MeetingsChanged,
    DevicesChanged {
        devices: Devices,
    },
    ConfigChanged {
        config: Config,
    },
    Error {
        message: String,
    },
    ShutdownComplete,
}
/// Implementations need not be Send: they are created, used and dropped on the
/// capture worker, preserving WASAPI apartment affinity.
pub trait Capture {
    fn monitor(&mut self, config: &Config) -> Result<(), String>;
    fn start(&mut self, dir: &Path, config: &Config) -> Result<(), String>;
    fn stop(&mut self) -> Result<crate::capture::Session, String>;
    fn levels(&mut self) -> (f32, f32);
    fn errors(&mut self) -> (Vec<String>, bool) {
        (vec![], false)
    }
    fn notices(&mut self) -> (Vec<crate::capture::CaptureNotice>, bool) {
        let (errors, fatal) = self.errors();
        (
            errors
                .into_iter()
                .map(|message| crate::capture::CaptureNotice {
                    level: crate::capture::NoticeLevel::Error,
                    message,
                    change: None,
                })
                .collect(),
            fatal,
        )
    }
    fn devices(&mut self) -> Result<Devices, String> {
        Ok(Devices::default())
    }
    /// Samples per-app system-audio levels while recording. The default is a
    /// no-op so test doubles and non-Windows backends need no change.
    fn sample_app_audio(&mut self, _config: &Config) {}
    /// Friendly name of the app that produced the most system audio during the
    /// current recording, if any. Default: unknown.
    fn source_app(&mut self) -> Option<String> {
        None
    }
    fn stop_monitor(&mut self);
}
pub trait Processor: Send + Sync + 'static {
    fn process(
        &self,
        dir: &Path,
        config: &Config,
        events: &crate::types::Events,
        abort: &Abort,
    ) -> Result<Meeting, String>;
}
struct AudioCapture {
    recorder: Option<crate::capture::Recorder>,
    monitor: Option<crate::capture::Monitor>,
    reported: usize,
    app: Option<crate::app_audio::Sampler>,
}
impl Capture for AudioCapture {
    fn monitor(&mut self, config: &Config) -> Result<(), String> {
        self.monitor = None;
        self.monitor = Some(crate::capture::Monitor::start(
            config.mic_device().into(),
            config.output_device().into(),
        )?);
        Ok(())
    }
    fn stop_monitor(&mut self) {
        self.monitor = None;
    }
    fn start(&mut self, dir: &Path, config: &Config) -> Result<(), String> {
        self.stop_monitor();
        self.reported = 0;
        self.recorder = Some(crate::capture::Recorder::start(
            dir,
            config.mic_device().into(),
            config.output_device().into(),
        )?);
        self.app = Some(crate::app_audio::Sampler::new());
        Ok(())
    }
    fn stop(&mut self) -> Result<crate::capture::Session, String> {
        self.app = None;
        self.recorder.take().ok_or("Not recording")?.stop()
    }
    fn sample_app_audio(&mut self, config: &Config) {
        if let Some(app) = &mut self.app {
            app.sample(config.output_device().as_deref(), Instant::now());
        }
    }
    fn source_app(&mut self) -> Option<String> {
        self.app.take().and_then(|app| app.top())
    }
    fn levels(&mut self) -> (f32, f32) {
        self.recorder
            .as_ref()
            .map(|r| r.levels())
            .or_else(|| self.monitor.as_ref().map(|m| m.levels()))
            .unwrap_or_default()
    }
    fn errors(&mut self) -> (Vec<String>, bool) {
        let Some(r) = &mut self.recorder else {
            return (
                self.monitor
                    .as_ref()
                    .map(|m| m.poll_errors())
                    .unwrap_or_default(),
                false,
            );
        };
        let (errors, fatal) = r.poll_errors();
        let fresh = errors[self.reported..].to_vec();
        self.reported = errors.len();
        (fresh, fatal)
    }
    fn notices(&mut self) -> (Vec<crate::capture::CaptureNotice>, bool) {
        if let Some(recorder) = &mut self.recorder {
            recorder.poll_notices()
        } else {
            (
                self.monitor
                    .as_ref()
                    .map(|m| m.poll_notices())
                    .unwrap_or_default(),
                false,
            )
        }
    }
    fn devices(&mut self) -> Result<Devices, String> {
        Ok(Devices {
            inputs: crate::devices::inputs()?,
            outputs: crate::devices::outputs()?,
        })
    }
}
struct Pipeline;
impl Processor for Pipeline {
    fn process(
        &self,
        dir: &Path,
        config: &Config,
        events: &crate::types::Events,
        abort: &Abort,
    ) -> Result<Meeting, String> {
        crate::pipeline::process(
            dir,
            &crate::pipeline::Options::from_config(config)?,
            events,
            abort,
        )
    }
}
struct Snapshot {
    config: Config,
    state: RecordingState,
    jobs: BTreeMap<PathBuf, JobState>,
    devices: Devices,
    monitoring: bool,
}
enum Command {
    Monitoring(bool),
    Start,
    Stop,
    Transcribe(PathBuf),
    Cancel(PathBuf),
    Devices,
    Config(Config),
    Mic(Option<String>),
    Output(Option<String>),
    Rename(PathBuf, String, Sender<Result<PathBuf, String>>),
    DeleteAudio(PathBuf),
    ArchiveMeeting(PathBuf),
    LibraryFinished(
        PathBuf,
        crate::library::Action,
        Result<Option<PathBuf>, String>,
    ),
    Shutdown,
}
struct Work {
    dir: PathBuf,
    config: Config,
    abort: Abort,
}
/// Monitoring starts paused. Opt in with set_monitoring(true); levels arrive at
/// 12.5 Hz while wanted. shutdown is idempotent; wait for ShutdownComplete before
/// closing. A cancelled blocking HTTP request does not delay audio shutdown.
pub struct Core {
    commands: Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
    closing: Arc<AtomicBool>,
    aborts: Arc<Mutex<BTreeMap<PathBuf, Abort>>>,
    worker: Option<JoinHandle<()>>,
}
impl Core {
    pub fn new() -> Result<(Self, Receiver<Event>), String> {
        Self::with_backends(
            Config::path(),
            || {
                crate::audio_thread_init();
                use cpal::traits::{DeviceTrait, HostTrait};
                let _ = cpal::default_host()
                    .default_output_device()
                    .map(|d| d.default_output_config());
                Box::new(AudioCapture {
                    recorder: None,
                    monitor: None,
                    reported: 0,
                    app: None,
                })
            },
            Arc::new(Pipeline),
        )
    }
    pub fn with_backends<F>(
        path: PathBuf,
        factory: F,
        processor: Arc<dyn Processor>,
    ) -> Result<(Self, Receiver<Event>), String>
    where
        F: FnOnce() -> Box<dyn Capture> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let (events, receiver) = mpsc::channel();
        let config = Config::load_at(&path);
        if let Err(e) = &config {
            let _ = events.send(Event::Error { message: e.clone() });
        }
        let snapshot = Arc::new(Mutex::new(Snapshot {
            config: config.unwrap_or_default(),
            state: RecordingState::Ready,
            jobs: BTreeMap::new(),
            devices: Devices::default(),
            monitoring: false,
        }));
        let closing = Arc::new(AtomicBool::new(false));
        let aborts = Arc::new(Mutex::new(BTreeMap::new()));
        let (ready_tx, ready_rx) = mpsc::channel();
        let actor_snapshot = snapshot.clone();
        let actor_closing = closing.clone();
        let actor_aborts = aborts.clone();
        let actor_commands = tx.clone();
        let worker = thread::Builder::new()
            .name("sottovoce-core-mta".into())
            .spawn(move || {
                let mut actor = Actor {
                    path,
                    snapshot: actor_snapshot,
                    closing: actor_closing,
                    aborts: actor_aborts,
                    events,
                    commands: actor_commands,
                    capture: Some(factory()),
                    dir: None,
                    started: Instant::now(),
                    processor,
                    queue: VecDeque::new(),
                    running: None,
                    library_busy: std::collections::BTreeSet::new(),
                    monitoring_wanted: false,
                    device_changes: Vec::new(),
                };
                let _ = ready_tx.send(());
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| actor.run(rx)));
                if result.is_err() {
                    actor.closing.store(true, Ordering::SeqCst);
                    actor.error("Core worker panicked");
                    actor.finish_shutdown();
                }
            })
            .map_err(|e| e.to_string())?;
        ready_rx
            .recv()
            .map_err(|_| "Capture initialization failed".to_string())?;
        Ok((
            Self {
                commands: tx,
                snapshot,
                closing,
                aborts,
                worker: Some(worker),
            },
            receiver,
        ))
    }
    fn send(&self, command: Command) -> Result<(), String> {
        if self.closing.load(Ordering::SeqCst) {
            return Err("Engine is shutting down".into());
        }
        self.commands
            .send(command)
            .map_err(|_| "Engine stopped".into())
    }
    pub fn start_recording(&self) -> Result<(), String> {
        self.send(Command::Start)
    }
    pub fn set_monitoring(&self, active: bool) -> Result<(), String> {
        self.send(Command::Monitoring(active))
    }
    pub fn monitoring(&self) -> bool {
        self.snapshot.lock().unwrap().monitoring
    }
    pub fn stop_recording(&self) -> Result<(), String> {
        self.send(Command::Stop)
    }
    pub fn transcribe(&self, meeting: PathBuf) -> Result<(), String> {
        self.send(Command::Transcribe(meeting))
    }
    pub fn cancel_job(&self, meeting: PathBuf) -> Result<(), String> {
        if let Some(abort) = self.aborts.lock().unwrap().get(&meeting) {
            abort.store(true, Ordering::SeqCst);
        }
        self.send(Command::Cancel(meeting))
    }
    pub fn list_meetings(&self) -> Vec<MeetingEntry> {
        let s = self.snapshot.lock().unwrap();
        crate::meetings::list(&s.config.meetings_dir())
            .into_iter()
            .map(|meeting| MeetingEntry {
                job: s.jobs.get(&meeting.dir).cloned(),
                meeting,
            })
            .collect()
    }
    pub fn jobs(&self) -> BTreeMap<PathBuf, JobState> {
        self.snapshot.lock().unwrap().jobs.clone()
    }
    pub fn recording_state(&self) -> RecordingState {
        self.snapshot.lock().unwrap().state
    }
    pub fn rename_meeting(&self, dir: PathBuf, title: String) -> Result<PathBuf, String> {
        let (tx, rx) = mpsc::channel();
        self.send(Command::Rename(dir, title, tx))?;
        rx.recv().map_err(|_| "Engine stopped".to_string())?
    }
    pub fn delete_audio(&self, meeting: PathBuf) -> Result<(), String> {
        self.send(Command::DeleteAudio(meeting))
    }
    pub fn archive_meeting(&self, meeting: PathBuf) -> Result<(), String> {
        self.send(Command::ArchiveMeeting(meeting))
    }
    pub fn set_mic_device(&self, id: Option<String>) -> Result<(), String> {
        self.send(Command::Mic(id))
    }
    pub fn set_output_device(&self, id: Option<String>) -> Result<(), String> {
        self.send(Command::Output(id))
    }
    pub fn list_devices(&self) -> Result<(), String> {
        self.send(Command::Devices)
    }
    pub fn get_config(&self) -> Config {
        self.snapshot.lock().unwrap().config.clone()
    }
    pub fn update_config(&self, config: Config) -> Result<(), String> {
        self.send(Command::Config(config))
    }
    pub fn shutdown(&self) -> Result<(), String> {
        if !self.closing.swap(true, Ordering::SeqCst) {
            for abort in self.aborts.lock().unwrap().values() {
                abort.store(true, Ordering::SeqCst);
            }
            self.commands
                .send(Command::Shutdown)
                .map_err(|_| "Engine stopped".to_string())?;
        }
        Ok(())
    }
}
impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct Running {
    dir: PathBuf,
    result: Receiver<Result<Meeting, String>>,
    progress: Receiver<crate::types::Event>,
    abort: Abort,
    worker: JoinHandle<()>,
    stage: String,
    fraction: f64,
}
struct Actor {
    path: PathBuf,
    snapshot: Arc<Mutex<Snapshot>>,
    closing: Arc<AtomicBool>,
    aborts: Arc<Mutex<BTreeMap<PathBuf, Abort>>>,
    events: Sender<Event>,
    commands: Sender<Command>,
    capture: Option<Box<dyn Capture>>,
    dir: Option<PathBuf>,
    started: Instant,
    processor: Arc<dyn Processor>,
    queue: VecDeque<Work>,
    running: Option<Running>,
    library_busy: std::collections::BTreeSet<PathBuf>,
    monitoring_wanted: bool,
    device_changes: Vec<crate::types::DeviceChange>,
}
impl Actor {
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
    fn error(&self, message: impl Into<String>) {
        self.emit(Event::Error {
            message: message.into(),
        });
    }
    fn config(&self) -> Config {
        self.snapshot.lock().unwrap().config.clone()
    }
    fn state(&self) -> RecordingState {
        self.snapshot.lock().unwrap().state
    }
    fn monitoring(&self) -> bool {
        self.snapshot.lock().unwrap().monitoring
    }
    fn audio(&mut self) -> &mut dyn Capture {
        self.capture.as_mut().unwrap().as_mut()
    }
    fn state_event(&self) {
        self.emit(Event::RecordingStateChanged {
            state: self.state(),
            elapsed_ms: if self.dir.is_some() {
                self.started.elapsed().as_millis() as u64
            } else {
                0
            },
            meeting: self.dir.clone(),
        });
    }
    fn transition(&self, next: RecordingState) -> Result<(), String> {
        {
            let mut s = self.snapshot.lock().unwrap();
            s.state = s.state.transition(next)?;
        }
        self.state_event();
        Ok(())
    }
    fn monitor(&mut self) {
        if !self.monitoring_wanted || self.state() != RecordingState::Ready {
            return;
        }
        let config = self.config();
        match self.audio().monitor(&config) {
            Ok(()) => self.monitoring_event(true),
            Err(e) => {
                self.monitoring_event(false);
                self.error(e);
            }
        }
    }
    fn monitoring_event(&self, active: bool) {
        self.snapshot.lock().unwrap().monitoring = active;
        self.emit(Event::MonitoringChanged { active });
        if !active {
            self.emit(Event::Levels {
                mic: 0.0,
                system: 0.0,
            });
        }
    }
    fn set_monitoring(&mut self, active: bool) {
        self.monitoring_wanted = active;
        if !active {
            self.audio().stop_monitor();
            self.monitoring_event(false);
        } else if self.state() == RecordingState::Ready && !self.monitoring() {
            self.monitor();
        }
    }
    fn poll_notices(&mut self) -> bool {
        let (notices, fatal) = self.audio().notices();
        for notice in notices {
            if self.state() == RecordingState::Recording {
                if let Some(change) = notice.change {
                    self.device_changes.push(change);
                }
                if notice.level == crate::capture::NoticeLevel::Info {
                    self.emit(Event::Notice {
                        message: notice.message,
                    });
                    continue;
                }
            }
            if notice.level == crate::capture::NoticeLevel::Error {
                self.error(notice.message);
            }
        }
        fatal
    }
    fn scan(&mut self) {
        match self.audio().devices() {
            Ok(devices) => {
                self.snapshot.lock().unwrap().devices = devices.clone();
                self.emit(Event::DevicesChanged { devices });
            }
            Err(e) => self.error(e),
        }
    }
    fn apply(&mut self, config: Config) {
        let old = self.config();
        if old == config {
            return;
        }
        let changed = old.mic_device() != config.mic_device()
            || old.output_device() != config.output_device();
        self.snapshot.lock().unwrap().config = config.clone();
        self.emit(Event::ConfigChanged { config });
        self.emit(Event::MeetingsChanged);
        if changed && self.state() == RecordingState::Ready {
            self.monitor();
        }
        // Recording uses its original device choices; new choices apply when it stops.
    }
    fn save_config(&mut self, config: Config) {
        match config.save_at(&self.path) {
            Ok(()) => self.apply(config),
            Err(e) => self.error(e),
        }
    }
    fn enqueue(&mut self, dir: PathBuf) -> Result<(), String> {
        if self.dir.as_ref() == Some(&dir) {
            return Err("Meeting is still recording".into());
        }
        if self.aborts.lock().unwrap().contains_key(&dir) {
            return Err("Meeting already queued or running".into());
        }
        if self.library_busy.contains(&dir) {
            return Err("Meeting is being deleted or archived".into());
        }
        if crate::meetings::entry(&dir).is_none() {
            return Err("Meeting does not exist".into());
        }
        let abort = Arc::new(AtomicBool::new(false));
        self.aborts
            .lock()
            .unwrap()
            .insert(dir.clone(), abort.clone());
        self.snapshot
            .lock()
            .unwrap()
            .jobs
            .insert(dir.clone(), JobState::Queued);
        self.queue.push_back(Work {
            dir: dir.clone(),
            config: self.config(),
            abort,
        });
        self.emit(Event::JobQueued { meeting: dir });
        self.emit(Event::MeetingsChanged);
        Ok(())
    }
    fn rename(&mut self, dir: PathBuf, title: &str) -> Result<PathBuf, String> {
        if self.dir.as_ref() == Some(&dir)
            || self.aborts.lock().unwrap().contains_key(&dir)
            || self.library_busy.contains(&dir)
        {
            return Err("Cannot rename an active meeting".into());
        }
        crate::meetings::rename(&dir, title)
    }
    fn begin_library(
        &mut self,
        dir: PathBuf,
        action: crate::library::Action,
    ) -> Result<(), String> {
        if self.dir.as_ref() == Some(&dir) {
            return Err("Cannot delete or archive a recording in progress".into());
        }
        if self.aborts.lock().unwrap().contains_key(&dir) {
            return Err("Cannot delete or archive a queued or running transcription".into());
        }
        if self.library_busy.contains(&dir) {
            return Err("Meeting is already being deleted or archived".into());
        }
        if crate::meetings::entry(&dir).is_none() {
            return Err("Meeting does not exist".into());
        }
        self.library_busy.insert(dir.clone());
        self.emit(Event::LibraryBusy {
            meeting: dir.clone(),
            busy: true,
        });
        let config = self.config();
        let commands = self.commands.clone();
        let worker_dir = dir.clone();
        let finished_dir = dir.clone();
        let worker = thread::Builder::new()
            .name("sottovoce-library-operation".into())
            .spawn(move || {
                let result = match action {
                    crate::library::Action::DeleteAudio => {
                        crate::library::delete_audio(&worker_dir, &config.transcripts_dir())
                    }
                    crate::library::Action::Archive => {
                        crate::library::archive(&worker_dir, &config.archive_dir()).map(Some)
                    }
                };
                let _ = commands.send(Command::LibraryFinished(finished_dir, action, result));
            });
        if let Err(error) = worker {
            self.library_busy.remove(&dir);
            self.emit(Event::LibraryBusy {
                meeting: dir,
                busy: false,
            });
            return Err(format!("Could not start library operation: {error}"));
        }
        Ok(())
    }
    fn terminal(&self, dir: PathBuf, result: Result<Meeting, String>) {
        let cancelled = self
            .aborts
            .lock()
            .unwrap()
            .remove(&dir)
            .is_some_and(|a| a.load(Ordering::SeqCst));
        let (state, event) = if cancelled
            || result
                .as_ref()
                .err()
                .is_some_and(|e| e == crate::types::CANCELLED)
        {
            (
                JobState::Cancelled,
                Event::JobCancelled {
                    meeting: dir.clone(),
                },
            )
        } else {
            match result {
                Ok(_) => (
                    JobState::Done,
                    Event::JobDone {
                        meeting: dir.clone(),
                    },
                ),
                Err(error) => (
                    JobState::Failed(error.clone()),
                    Event::JobFailed {
                        meeting: dir.clone(),
                        error,
                    },
                ),
            }
        };
        self.snapshot.lock().unwrap().jobs.insert(dir, state);
        self.emit(event);
        self.emit(Event::MeetingsChanged);
    }
    fn tick_jobs(&mut self) {
        if let Some(r) = &mut self.running {
            while let Ok(event) = r.progress.try_recv() {
                match event {
                    crate::types::Event::Stage(s) => {
                        r.stage = s;
                        r.fraction = 0.0;
                    }
                    crate::types::Event::Progress(p) => r.fraction = p.clamp(0.0, 1.0),
                    crate::types::Event::Log(s) => r.stage = s,
                }
                let _ = self.events.send(Event::JobProgress {
                    meeting: r.dir.clone(),
                    stage: r.stage.clone(),
                    progress: r.fraction,
                });
            }
            let result = match r.result.try_recv() {
                Ok(v) => Some(v),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Job worker disconnected".into()))
                }
                Err(_) => None,
            };
            if let Some(result) = result {
                let r = self.running.take().unwrap();
                let _ = r.worker.join();
                self.terminal(r.dir, result);
            }
        }
        if self.running.is_none() && !self.closing.load(Ordering::SeqCst) {
            while let Some(work) = self.queue.pop_front() {
                if work.abort.load(Ordering::SeqCst) {
                    self.terminal(work.dir, Err(crate::types::CANCELLED.into()));
                    continue;
                }
                let (tx, result) = mpsc::channel();
                let (events, progress) = mpsc::channel();
                let dir = work.dir.clone();
                let abort = work.abort.clone();
                let processor = self.processor.clone();
                let worker = thread::Builder::new()
                    .name("sottovoce-transcription".into())
                    .spawn(move || {
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                processor.process(&work.dir, &work.config, &events, &work.abort)
                            }))
                            .unwrap_or_else(|_| Err("Transcription worker panicked".into()));
                        let _ = tx.send(outcome);
                    });
                match worker {
                    Ok(worker) => {
                        self.snapshot
                            .lock()
                            .unwrap()
                            .jobs
                            .insert(dir.clone(), JobState::Running);
                        self.emit(Event::JobProgress {
                            meeting: dir.clone(),
                            stage: "Starting".into(),
                            progress: 0.0,
                        });
                        self.running = Some(Running {
                            dir,
                            result,
                            progress,
                            abort,
                            worker,
                            stage: "Starting".into(),
                            fraction: 0.0,
                        });
                    }
                    Err(e) => self.terminal(dir, Err(e.to_string())),
                }
                break;
            }
        }
    }
    fn start(&mut self) -> Result<(), String> {
        self.transition(RecordingState::Starting)?;
        self.audio().stop_monitor();
        self.monitoring_event(false);
        self.device_changes.clear();
        let config = self.config();
        let dir = crate::meetings::new_dir(&config.meetings_dir(), crate::capture::unix_ms());
        match self.audio().start(&dir, &config) {
            Ok(()) => {
                self.dir = Some(dir.clone());
                self.started = Instant::now();
                if let Err(e) =
                    std::fs::write(dir.join(".recorder-pid"), std::process::id().to_string())
                {
                    self.error(e.to_string());
                }
                self.transition(RecordingState::Recording)?;
                self.emit(Event::MeetingsChanged);
            }
            Err(e) => {
                crate::meetings::cleanup_failed_start(&dir);
                self.transition(RecordingState::Ready)?;
                if !self.closing.load(Ordering::SeqCst) {
                    self.monitor();
                }
                return Err(e);
            }
        }
        Ok(())
    }
    fn stop(&mut self) -> Result<(), String> {
        let dir = self.dir.clone().ok_or("No recording directory")?;
        self.poll_notices();
        self.transition(RecordingState::Finalizing)?;
        let source_app = self.audio().source_app();
        let result = self.audio().stop().and_then(|session| {
            let duration = session
                .stopped_at_unix_ms
                .unwrap_or(session.started_at_unix_ms)
                - session.started_at_unix_ms;
            let title = source_app
                .clone()
                .unwrap_or_else(|| crate::meetings::default_title(session.started_at_unix_ms));
            crate::meetings::save(
                &dir,
                &Meeting {
                    title,
                    source_app: source_app.clone(),
                    started_at_unix_ms: session.started_at_unix_ms,
                    duration_ms: duration,
                    device_changes: if session.device_changes.is_empty() {
                        self.device_changes.clone()
                    } else {
                        session.device_changes.clone()
                    },
                    ..Default::default()
                },
            )?;
            if session.status != "completed" {
                return Err(format!("Recording failed: {}", session.errors.join("; ")));
            }
            Ok(())
        });
        let _ = std::fs::remove_file(dir.join(".recorder-pid"));
        self.dir = None;
        self.transition(RecordingState::Ready)?;
        self.emit(Event::MeetingsChanged);
        if !self.closing.load(Ordering::SeqCst) {
            self.monitor();
            if result.is_ok() && self.config().auto_transcribe() {
                if let Err(e) = self.enqueue(dir) {
                    self.error(e);
                }
            }
        }
        result
    }
    fn run(&mut self, commands: Receiver<Command>) {
        let mut modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        crate::meetings::recover(&self.config().meetings_dir());
        self.emit(Event::MeetingsChanged);
        self.emit(Event::ConfigChanged {
            config: self.config(),
        });
        self.state_event();
        self.scan();
        self.monitoring_event(false);
        let mut levels_at = Instant::now();
        let mut poll = Instant::now();
        let mut app_at = Instant::now();
        loop {
            if self.closing.load(Ordering::SeqCst) {
                break;
            }
            let active = self.monitoring()
                || self.state() == RecordingState::Recording
                || self.running.is_some();
            match commands.recv_timeout(if active {
                Duration::from_millis(80)
            } else {
                Duration::from_secs(1)
            }) {
                Ok(command) => {
                    if self.closing.load(Ordering::SeqCst) {
                        break;
                    }
                    let result = match command {
                        Command::Monitoring(active) => {
                            self.set_monitoring(active);
                            Ok(())
                        }
                        Command::Start => self.start(),
                        Command::Stop => self.stop(),
                        Command::Transcribe(dir) => self.enqueue(dir),
                        Command::Cancel(dir) => {
                            let abort = self.aborts.lock().unwrap().get(&dir).cloned();
                            if let Some(abort) = abort {
                                abort.store(true, Ordering::SeqCst);
                                if let Some(i) = self.queue.iter().position(|w| w.dir == dir) {
                                    self.queue.remove(i);
                                    self.terminal(dir, Err(crate::types::CANCELLED.into()));
                                } else {
                                    self.snapshot
                                        .lock()
                                        .unwrap()
                                        .jobs
                                        .insert(dir, JobState::Cancelling);
                                }
                                Ok(())
                            } else {
                                Err("No active job for meeting".into())
                            }
                        }
                        Command::Devices => {
                            self.scan();
                            Ok(())
                        }
                        Command::Config(c) => {
                            self.save_config(c);
                            Ok(())
                        }
                        Command::Mic(id) => {
                            let mut c = self.config();
                            c.mic_device = id;
                            self.save_config(c);
                            Ok(())
                        }
                        Command::Output(id) => {
                            let mut c = self.config();
                            c.output_device = id;
                            self.save_config(c);
                            Ok(())
                        }
                        Command::Rename(dir, title, reply) => {
                            let result = self.rename(dir, &title);
                            if let Err(e) = &result {
                                self.error(e.clone());
                            } else {
                                self.emit(Event::MeetingsChanged);
                            }
                            let _ = reply.send(result);
                            Ok(())
                        }
                        Command::DeleteAudio(dir) => {
                            self.begin_library(dir, crate::library::Action::DeleteAudio)
                        }
                        Command::ArchiveMeeting(dir) => {
                            self.begin_library(dir, crate::library::Action::Archive)
                        }
                        Command::LibraryFinished(dir, action, result) => {
                            self.library_busy.remove(&dir);
                            self.emit(Event::LibraryBusy {
                                meeting: dir.clone(),
                                busy: false,
                            });
                            match result {
                                Ok(output) => self.emit(Event::LibraryDone {
                                    meeting: dir,
                                    action,
                                    output,
                                }),
                                Err(message) => self.error(message),
                            }
                            self.emit(Event::MeetingsChanged);
                            Ok(())
                        }
                        Command::Shutdown => break,
                    };
                    if let Err(e) = result {
                        self.error(e);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.closing.store(true, Ordering::SeqCst);
                    break;
                }
                Err(_) => (),
            }
            if (self.monitoring() || self.state() == RecordingState::Recording)
                && levels_at.elapsed() >= Duration::from_millis(80)
            {
                levels_at = Instant::now();
                // While recording, levels come from the recorder's own streams,
                // so the sidebar meters work from any screen at no extra cost.
                let (mic, system) = self.audio().levels();
                self.emit(Event::Levels { mic, system });
                if self.state() == RecordingState::Recording {
                    self.state_event();
                }
            }
            if app_at.elapsed() >= Duration::from_secs(1) {
                app_at = Instant::now();
                if self.state() == RecordingState::Recording {
                    let config = self.config();
                    self.audio().sample_app_audio(&config);
                }
            }
            let fatal = if self.monitoring() || self.state() == RecordingState::Recording {
                self.poll_notices()
            } else {
                false
            };
            if fatal && self.state() == RecordingState::Recording {
                if let Err(e) = self.stop() {
                    self.error(e);
                }
            }
            self.tick_jobs();
            if poll.elapsed() >= Duration::from_secs(1) {
                poll = Instant::now();
                let next = std::fs::metadata(&self.path)
                    .and_then(|m| m.modified())
                    .ok();
                if next != modified {
                    modified = next;
                    match Config::load_at(&self.path) {
                        Ok(config) => self.apply(config),
                        Err(e) => self.error(e),
                    }
                }
            }
        }
        self.finish_shutdown();
    }
    fn finish_shutdown(&mut self) {
        if self.state() == RecordingState::Recording {
            if let Err(e) = self.stop() {
                self.error(e);
            }
        }
        self.audio().stop_monitor();
        self.monitoring_event(false);
        while let Some(work) = self.queue.pop_front() {
            work.abort.store(true, Ordering::SeqCst);
            self.terminal(work.dir, Err(crate::types::CANCELLED.into()));
        }
        if let Some(r) = self.running.take() {
            r.abort.store(true, Ordering::SeqCst);
            // Blocking HTTP/ONNX cannot be forcibly interrupted. Drop the join
            // handle, never wait indefinitely. Pipeline checks abort before save.
            self.terminal(r.dir, Err(crate::types::CANCELLED.into()));
        }
        self.capture = None;
        self.emit(Event::ShutdownComplete);
    }
}
#[cfg(test)]
mod tests;
