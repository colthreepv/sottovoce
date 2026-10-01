use super::*;
use std::sync::atomic::AtomicUsize;
static SEQ: AtomicUsize = AtomicUsize::new(0);
struct FakeCapture {
    started: i64,
    dir: Option<PathBuf>,
    fail_start: bool,
    fail_stop: bool,
    stops: Arc<AtomicUsize>,
}
impl Capture for FakeCapture {
    fn monitor(&mut self, _: &Config) -> Result<(), String> {
        Ok(())
    }
    fn stop_monitor(&mut self) {}
    fn start(&mut self, dir: &Path, _: &Config) -> Result<(), String> {
        if self.fail_start {
            return Err("fake start failure".into());
        }
        std::fs::create_dir_all(dir).unwrap();
        self.started = crate::capture::unix_ms();
        self.dir = Some(dir.to_owned());
        crate::capture::Session {
            status: "recording".into(),
            started_at_unix_ms: self.started,
            ..Default::default()
        }
        .save(dir)
    }
    fn stop(&mut self) -> Result<crate::capture::Session, String> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if self.fail_stop {
            return Err("fake finalize failure".into());
        }
        let session = crate::capture::Session {
            status: "completed".into(),
            started_at_unix_ms: self.started,
            stopped_at_unix_ms: Some(self.started + 100),
            ..Default::default()
        };
        session.save(&self.dir.take().unwrap())?;
        Ok(session)
    }
    fn levels(&mut self) -> (f32, f32) {
        (0.1, 0.2)
    }
}
struct FakeProcessor {
    started: Sender<PathBuf>,
    release: Mutex<Receiver<()>>,
    fail: bool,
}
impl Processor for FakeProcessor {
    fn process(
        &self,
        dir: &Path,
        _: &Config,
        events: &crate::types::Events,
        abort: &Abort,
    ) -> Result<Meeting, String> {
        self.started.send(dir.to_owned()).unwrap();
        let _ = events.send(crate::types::Event::Stage("fake processing".into()));
        loop {
            if abort.load(Ordering::SeqCst) {
                return Err(crate::types::CANCELLED.into());
            }
            if self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_millis(10))
                .is_ok()
            {
                break;
            }
        }
        if self.fail {
            Err("fake pipeline failure".into())
        } else {
            Ok(Meeting::default())
        }
    }
}
struct Harness {
    core: Core,
    events: Receiver<Event>,
    started: Receiver<PathBuf>,
    release: Sender<()>,
    root: PathBuf,
    stops: Arc<AtomicUsize>,
}
impl Harness {
    fn new(fail_start: bool, fail_stop: bool, fail_job: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "sottovoce-core-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Config {
            meetings_dir: Some(root.join("meetings")),
            transcripts_dir: Some(root.join("transcripts")),
            archive_dir: Some(root.join("archive")),
            ..Default::default()
        }
        .save_at(&root.join("config.toml"))
        .unwrap();
        let (tx, started) = mpsc::channel();
        let (release, rx) = mpsc::channel();
        let stops = Arc::new(AtomicUsize::new(0));
        let captured = stops.clone();
        let (core, events) = Core::with_backends(
            root.join("config.toml"),
            move || {
                Box::new(FakeCapture {
                    started: 0,
                    dir: None,
                    fail_start,
                    fail_stop,
                    stops: captured,
                })
            },
            Arc::new(FakeProcessor {
                started: tx,
                release: Mutex::new(rx),
                fail: fail_job,
            }),
        )
        .unwrap();
        let h = Self {
            core,
            events,
            started,
            release,
            root,
            stops,
        };
        h.wait(|e| {
            matches!(
                e,
                Event::RecordingStateChanged {
                    state: RecordingState::Ready,
                    ..
                }
            )
        });
        h
    }
    fn wait(&self, predicate: impl Fn(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let e = self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("event deadline");
            if predicate(&e) {
                return e;
            }
        }
    }
    fn recording(&self) -> PathBuf {
        self.core.start_recording().unwrap();
        match self.wait(|e| {
            matches!(
                e,
                Event::RecordingStateChanged {
                    state: RecordingState::Recording,
                    ..
                }
            )
        }) {
            Event::RecordingStateChanged { meeting, .. } => meeting.unwrap(),
            _ => unreachable!(),
        }
    }
    fn stop(&self) {
        self.core.stop_recording().unwrap();
        self.wait(|e| {
            matches!(
                e,
                Event::RecordingStateChanged {
                    state: RecordingState::Ready,
                    ..
                }
            )
        });
    }
    fn close(&self) {
        self.core.shutdown().unwrap();
        self.wait(|e| matches!(e, Event::ShutdownComplete));
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.core.shutdown();
        if let Some(w) = self.core.worker.take() {
            let _ = w.join();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn library_busy_meetings_refuse_rename_transcription_and_second_operation() {
    let h = Harness::new(false, false, false);
    let dir = h.root.join("meetings").join("meeting");
    std::fs::create_dir_all(&dir).unwrap();
    crate::capture::Session {
        status: "completed".into(),
        started_at_unix_ms: 1,
        stopped_at_unix_ms: Some(2),
        ..Default::default()
    }
    .save(&dir)
    .unwrap();
    crate::meetings::save(
        &dir,
        &Meeting {
            title: "Busy test".into(),
            started_at_unix_ms: 1,
            ..Default::default()
        },
    )
    .unwrap();
    std::fs::write(dir.join("mic.ogg"), b"mic").unwrap();
    std::fs::write(dir.join("computer.ogg"), b"computer").unwrap();
    let (events, event_receiver) = mpsc::channel();
    let (commands, command_receiver) = mpsc::channel();
    let mut actor = Actor {
        path: h.root.join("config.toml"),
        snapshot: Arc::new(Mutex::new(Snapshot {
            config: h.core.get_config(),
            state: RecordingState::Ready,
            jobs: BTreeMap::new(),
            devices: Devices::default(),
        })),
        closing: Arc::new(AtomicBool::new(false)),
        aborts: Arc::new(Mutex::new(BTreeMap::new())),
        events,
        commands,
        capture: None,
        dir: None,
        started: Instant::now(),
        processor: Arc::new(Pipeline),
        queue: VecDeque::new(),
        running: None,
        library_busy: std::collections::BTreeSet::new(),
    };
    actor
        .begin_library(dir.clone(), crate::library::Action::Archive)
        .unwrap();
    assert!(matches!(
        event_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
        Event::LibraryBusy { meeting, busy: true } if meeting == dir
    ));
    assert!(actor.rename(dir.clone(), "Renamed").is_err());
    assert!(actor.enqueue(dir.clone()).is_err());
    assert!(
        actor
            .begin_library(dir.clone(), crate::library::Action::DeleteAudio)
            .is_err()
    );
    assert!(matches!(
        command_receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
        Command::LibraryFinished(meeting, crate::library::Action::Archive, Ok(Some(_)))
            if meeting == dir
    ));
    h.close();
}
#[test]
fn transition_table_is_explicit() {
    use RecordingState::*;
    for a in [Ready, Starting, Recording, Finalizing] {
        for b in [Ready, Starting, Recording, Finalizing] {
            assert_eq!(
                a.transition(b).is_ok(),
                matches!(
                    (a, b),
                    (Ready, Starting)
                        | (Starting, Recording)
                        | (Starting, Ready)
                        | (Recording, Finalizing)
                        | (Finalizing, Ready)
                )
            );
        }
    }
}
#[test]
fn fifo_back_to_back_recording_and_queued_cancel() {
    let h = Harness::new(false, false, false);
    let first = h.recording();
    h.stop();
    h.core.transcribe(first.clone()).unwrap();
    assert_eq!(
        h.started.recv_timeout(Duration::from_secs(3)).unwrap(),
        first
    );
    let second = h.recording();
    h.stop();
    assert_ne!(first, second);
    h.core.transcribe(second.clone()).unwrap();
    h.wait(|e| matches!(e, Event::JobQueued { meeting } if meeting == &second));
    assert_eq!(h.core.jobs()[&first], JobState::Running);
    assert_eq!(h.core.jobs()[&second], JobState::Queued);
    assert!(h.started.try_recv().is_err());
    h.core.cancel_job(second.clone()).unwrap();
    h.wait(|e| matches!(e, Event::JobCancelled { meeting } if meeting == &second));
    h.release.send(()).unwrap();
    h.wait(|e| matches!(e, Event::JobDone { meeting } if meeting == &first));
    h.core.transcribe(second.clone()).unwrap();
    assert_eq!(
        h.started.recv_timeout(Duration::from_secs(3)).unwrap(),
        second
    );
    h.core.cancel_job(second.clone()).unwrap();
    h.wait(|e| matches!(e, Event::JobCancelled { meeting } if meeting == &second));
    h.close();
}
#[test]
fn fifo_runs_next_job_only_after_previous_finishes() {
    let h = Harness::new(false, false, false);
    let a = h.recording();
    h.stop();
    let b = h.recording();
    h.stop();
    h.core.transcribe(a.clone()).unwrap();
    h.core.transcribe(b.clone()).unwrap();
    assert_eq!(h.started.recv_timeout(Duration::from_secs(3)).unwrap(), a);
    assert!(h.started.try_recv().is_err());
    h.release.send(()).unwrap();
    assert_eq!(h.started.recv_timeout(Duration::from_secs(3)).unwrap(), b);
    h.release.send(()).unwrap();
    h.wait(|e| matches!(e, Event::JobDone { meeting } if meeting == &b));
    h.close();
}
#[test]
fn shutdown_finalizes_and_cancels_without_close_loop() {
    let h = Harness::new(false, false, false);
    let first = h.recording();
    h.stop();
    h.core.transcribe(first).unwrap();
    h.started.recv_timeout(Duration::from_secs(3)).unwrap();
    let second = h.recording();
    h.close();
    h.core.shutdown().unwrap();
    assert_eq!(h.stops.load(Ordering::SeqCst), 2);
    assert_eq!(
        crate::capture::Session::load(&second).unwrap().status,
        "completed"
    );
    assert!(h.core.start_recording().is_err());
    assert!(h.core.jobs().values().all(|s| *s == JobState::Cancelled));
}
#[test]
fn failures_return_recording_to_ready_and_report_jobs() {
    let h = Harness::new(true, false, false);
    h.core.start_recording().unwrap();
    h.wait(|e| matches!(e, Event::Error { message } if message.contains("fake start")));
    assert_eq!(h.core.recording_state(), RecordingState::Ready);
    h.close();
    let h = Harness::new(false, true, false);
    h.recording();
    h.core.stop_recording().unwrap();
    h.wait(|e| matches!(e, Event::Error { message } if message.contains("fake finalize")));
    assert_eq!(h.core.recording_state(), RecordingState::Ready);
    h.close();
    let h = Harness::new(false, false, true);
    let dir = h.recording();
    h.stop();
    h.core.transcribe(dir).unwrap();
    h.started.recv_timeout(Duration::from_secs(3)).unwrap();
    h.release.send(()).unwrap();
    h.wait(|e| matches!(e, Event::JobFailed { error, .. } if error == "fake pipeline failure"));
    h.close();
}
#[test]
fn config_reload_keeps_last_good_on_invalid_edits() {
    let h = Harness::new(false, false, false);
    // Discard startup ConfigChanged before waiting for manual changes.
    while h.events.try_recv().is_ok() {}
    let mut config = h.core.get_config();
    config.your_name = Some("Manual edit".into());
    config.save_at(&h.root.join("config.toml")).unwrap();
    h.wait(|e| matches!(e, Event::ConfigChanged { config } if config.your_name.as_deref() == Some("Manual edit")));
    std::fs::write(h.root.join("config.toml"), "diarize = [invalid").unwrap();
    h.wait(|e| matches!(e, Event::Error { message } if message.contains("Invalid config")));
    assert_eq!(h.core.get_config(), config);
    config.your_name = Some("Repaired".into());
    config.save_at(&h.root.join("config.toml")).unwrap();
    h.wait(|e| matches!(e, Event::ConfigChanged { config } if config.your_name.as_deref() == Some("Repaired")));
    h.close();
}
#[test]
fn default_template_and_atomic_replacement_roundtrip() {
    let root = std::env::temp_dir().join(format!(
        "sottovoce-config-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let path = root.join("config.toml");
    let mut c = Config::load_at(&path).unwrap();
    assert!(!c.auto_transcribe());
    assert!(std::fs::read_to_string(&path).unwrap().starts_with('#'));
    c.your_name = Some("First".into());
    c.save_at(&path).unwrap();
    c.your_name = Some("Second".into());
    c.save_at(&path).unwrap();
    assert_eq!(Config::load_at(&path).unwrap(), c);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn auto_transcription_is_separate_from_recording() {
    let h = Harness::new(false, false, false);
    let mut c = h.core.get_config();
    c.auto_transcribe = Some(true);
    h.core.update_config(c).unwrap();
    h.wait(|e| matches!(e, Event::ConfigChanged { config } if config.auto_transcribe()));
    let first = h.recording();
    h.stop();
    assert_eq!(
        h.started.recv_timeout(Duration::from_secs(3)).unwrap(),
        first
    );
    h.recording();
    h.close();
}

#[test]
fn shutdown_during_start_finalizes_before_acknowledging() {
    struct StartingCapture {
        inner: FakeCapture,
        entered: Sender<()>,
        release: Receiver<()>,
    }
    impl Capture for StartingCapture {
        fn monitor(&mut self, _: &Config) -> Result<(), String> {
            Ok(())
        }
        fn stop_monitor(&mut self) {}
        fn start(&mut self, dir: &Path, c: &Config) -> Result<(), String> {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            self.inner.start(dir, c)
        }
        fn stop(&mut self) -> Result<crate::capture::Session, String> {
            self.inner.stop()
        }
        fn levels(&mut self) -> (f32, f32) {
            (0.0, 0.0)
        }
    }
    let root = std::env::temp_dir().join(format!(
        "sottovoce-start-shutdown-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let path = root.join("config.toml");
    Config {
        meetings_dir: Some(root.join("meetings")),
        ..Default::default()
    }
    .save_at(&path)
    .unwrap();
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let stops = Arc::new(AtomicUsize::new(0));
    let worker_stops = stops.clone();
    let (job_tx, _) = mpsc::channel();
    let (_, job_rx) = mpsc::channel();
    let (mut core, events) = Core::with_backends(
        path,
        move || {
            Box::new(StartingCapture {
                inner: FakeCapture {
                    started: 0,
                    dir: None,
                    fail_start: false,
                    fail_stop: false,
                    stops: worker_stops,
                },
                entered: entered_tx,
                release: release_rx,
            })
        },
        Arc::new(FakeProcessor {
            started: job_tx,
            release: Mutex::new(job_rx),
            fail: false,
        }),
    )
    .unwrap();
    core.start_recording().unwrap();
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    core.shutdown().unwrap();
    assert_eq!(core.recording_state(), RecordingState::Starting);
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if matches!(
            events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap(),
            Event::ShutdownComplete
        ) {
            break;
        }
    }
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(core.recording_state(), RecordingState::Ready);
    core.worker.take().unwrap().join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
