//! The Windows meeting recorder interface.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use eframe::egui::{self, Align, Color32, FontId, Layout, RichText, Sense, Vec2};

use crate::config::Config;
use crate::core::{Core, Event as CoreEvent, RecordingState};
use crate::devices::AudioDevice;
use crate::meetings::{self, Entry};
use crate::player::Player;
use crate::types::{Meeting, Side};

pub fn run() -> Result<(), String> {
    // Initialize CPAL's cached enumerator in a live MTA before winit.
    let (core, core_events) = Core::new()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Meeting Recorder")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([850.0, 570.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Meeting Recorder",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc, core, core_events)))),
    )
    .map_err(|e| format!("could not open Meeting Recorder: {e}"))
}

#[derive(Clone)]
enum Screen {
    Recording,
    Meeting(PathBuf),
    Settings,
}

// Presentation-only snapshots. Audio handles never cross into the UI.
struct RecordingView {
    elapsed: Duration,
}
impl RecordingView {
    fn elapsed(&self) -> Duration {
        self.elapsed
    }
    fn mic_device(&self) -> &str {
        "Selected microphone"
    }
    fn computer_device(&self) -> &str {
        "Selected output"
    }
}
struct Job {
    dir: PathBuf,
    stage: String,
    progress: f64,
    log: VecDeque<String>,
    finalizing: bool,
}

struct App {
    config: Config,
    entries: Vec<Entry>,
    screen: Screen,
    core: Core,
    core_events: Receiver<CoreEvent>,
    state: RecordingState,
    shutting_down: bool,
    recorder: Option<RecordingView>,
    starting: Option<()>,
    allow_close: bool,
    folder_picker: Option<Receiver<Option<PathBuf>>>,
    test_seconds: Option<u64>,
    test_started: bool,
    settings_auto_transcribe: bool,
    input_devices: Vec<AudioDevice>,
    output_devices: Vec<AudioDevice>,
    device_popups_open: [bool; 2],

    idle_meters: [f32; 2],
    recording_meters: [f32; 2],
    recording_elapsed: Duration,
    recording_errors: Vec<String>,
    job: Option<Job>,
    player: Option<Player>,
    player_dir: Option<PathBuf>,
    meeting_cache: Option<(PathBuf, Meeting)>,
    saved_title: String,
    meeting_dirty: bool,
    notice: Option<String>,
    settings_key: String,
    settings_show_key: bool,
    settings_name: String,
    settings_meetings_dir: String,
    settings_diarize: bool,
    settings_message: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, core: Core, core_events: Receiver<CoreEvent>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::from_rgb(23, 26, 32);
        visuals.window_fill = Color32::from_rgb(28, 32, 39);
        visuals.extreme_bg_color = Color32::from_rgb(16, 19, 24);
        visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(34, 39, 47);
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(43, 49, 59);
        visuals.selection.bg_fill = Color32::from_rgb(45, 105, 112);
        cc.egui_ctx.set_visuals(visuals);
        let config = core.get_config();
        if let Ok(dir) = std::env::var("MR_GUI_TEST_DIR") {
            let mut config = config.clone();
            config.meetings_dir = Some(PathBuf::from(dir));
            config.auto_transcribe = Some(false);
            let _ = core.update_config(config);
        }
        let mut app = Self {
            entries: meetings::list(&config.meetings_dir()),
            settings_key: config.elevenlabs_api_key.clone().unwrap_or_default(),
            settings_show_key: false,
            settings_name: config.your_name.clone().unwrap_or_default(),
            settings_meetings_dir: config.meetings_dir().display().to_string(),
            settings_diarize: config.diarize(),
            config: config.clone(),
            screen: Screen::Recording,
            core,
            core_events,
            state: RecordingState::Ready,
            shutting_down: false,
            recorder: None,
            starting: None,
            allow_close: false,
            folder_picker: None,
            test_seconds: std::env::var("MR_GUI_RECORD_TEST_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok()),
            test_started: false,
            settings_auto_transcribe: config.auto_transcribe(),
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            device_popups_open: [false; 2],

            idle_meters: [0.0; 2],
            recording_meters: [0.0; 2],
            recording_elapsed: Duration::ZERO,
            recording_errors: Vec::new(),
            job: None,
            player: None,
            player_dir: None,
            meeting_cache: None,
            saved_title: String::new(),
            meeting_dirty: false,
            notice: None,
            settings_message: None,
        };
        app.refresh_meetings();
        app.refresh_devices();
        app
    }

    fn refresh_meetings(&mut self) {
        self.entries = self
            .core
            .list_meetings()
            .into_iter()
            .map(|e| {
                let mut entry = e.meeting;
                if let Some(job) = e.job {
                    entry.status = format!("{job:?}");
                }
                entry
            })
            .collect();
    }
    fn refresh_devices(&mut self) {
        if let Err(e) = self.core.list_devices() {
            self.notice = Some(e);
        }
    }
    fn select_meeting(&mut self, dir: PathBuf) {
        self.screen = Screen::Meeting(dir);
        self.notice = None;
    }
    fn arm_recording(&mut self) {
        self.screen = Screen::Recording;
    }
    fn start_recording(&mut self) {
        if let Err(e) = self.core.start_recording() {
            self.notice = Some(e);
        }
        self.screen = Screen::Recording;
    }
    fn stop_recording(&mut self, close: bool) {
        let result = if close {
            self.shutting_down = true;
            self.core.shutdown()
        } else {
            self.core.stop_recording()
        };
        if let Err(e) = result {
            self.notice = Some(e);
        }
    }
    fn start_transcription(&mut self, dir: PathBuf) {
        if let Err(e) = self.core.transcribe(dir) {
            self.notice = Some(e);
        }
    }
    fn poll_background(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.folder_picker {
            match rx.try_recv() {
                Ok(path) => {
                    if let Some(path) = path {
                        self.settings_meetings_dir = path.display().to_string();
                    }
                    self.folder_picker = None;
                }
                Err(TryRecvError::Disconnected) => self.folder_picker = None,
                Err(TryRecvError::Empty) => (),
            }
        }
        while let Ok(event) = self.core_events.try_recv() {
            match event {
                CoreEvent::Levels { mic, system } => {
                    self.idle_meters = [
                        smooth_level(self.idle_meters[0], mic),
                        smooth_level(self.idle_meters[1], system),
                    ];
                    self.recording_meters = self.idle_meters;
                }
                CoreEvent::RecordingStateChanged {
                    state, elapsed_ms, ..
                } => {
                    self.state = state;
                    self.starting = (state == RecordingState::Starting).then_some(());
                    self.recording_elapsed = Duration::from_millis(elapsed_ms);
                    self.recorder = (state == RecordingState::Recording).then_some(RecordingView {
                        elapsed: self.recording_elapsed,
                    });
                }
                CoreEvent::DevicesChanged { devices } => {
                    self.input_devices = devices.inputs;
                    self.output_devices = devices.outputs;
                }
                CoreEvent::ConfigChanged { config } => {
                    self.config = config;
                    if matches!(self.screen, Screen::Settings) {
                        self.load_settings_draft();
                    }
                    self.refresh_meetings();
                }
                CoreEvent::MeetingsChanged | CoreEvent::JobQueued { .. } => self.refresh_meetings(),
                CoreEvent::JobProgress {
                    meeting,
                    stage,
                    progress,
                } => {
                    self.job = Some(Job {
                        dir: meeting,
                        stage,
                        progress,
                        log: VecDeque::new(),
                        finalizing: false,
                    });
                }
                CoreEvent::JobDone { meeting } => {
                    if self.job.as_ref().is_some_and(|j| j.dir == meeting) {
                        self.job = None;
                    }
                    self.meeting_cache = None;
                    self.refresh_meetings();
                }
                CoreEvent::JobFailed { meeting, error } => {
                    if self.job.as_ref().is_some_and(|j| j.dir == meeting) {
                        self.job = None;
                    }
                    self.notice = Some(error);
                    self.refresh_meetings();
                }
                CoreEvent::JobCancelled { meeting } => {
                    if self.job.as_ref().is_some_and(|j| j.dir == meeting) {
                        self.job = None;
                    }
                    self.refresh_meetings();
                }
                CoreEvent::Error { message } => self.notice = Some(message),
                CoreEvent::ShutdownComplete => {
                    self.allow_close = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn show_sidebar(&mut self, root_ui: &mut egui::Ui) {
        let mut new_recording = false;
        let mut open_settings = false;
        let mut refresh = false;
        let mut picked = None;
        egui::Panel::left("meeting-list")
            .exact_size(270.0)
            .resizable(false)
            .show(root_ui, |ui| {
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("Meeting Recorder").size(17.0));
                    if ui
                        .small_button("Refresh")
                        .on_hover_text("Refresh meetings")
                        .clicked()
                    {
                        refresh = true;
                    }
                });
                ui.add_space(14.0);
                let can_open = !self.shutting_down;
                let label = if self.recorder.is_some() {
                    "Show recording"
                } else {
                    "New recording"
                };
                if ui
                    .add_enabled(
                        can_open,
                        egui::Button::new(RichText::new(label).strong())
                            .min_size(Vec2::new(ui.available_width(), 42.0)),
                    )
                    .clicked()
                {
                    new_recording = true;
                }
                if self.recorder.is_some() {
                    ui.label(
                        RichText::new("Recording in progress")
                            .color(Color32::from_rgb(235, 125, 104)),
                    );
                }
                ui.add_space(18.0);
                ui.label(
                    RichText::new("MEETINGS")
                        .size(11.0)
                        .strong()
                        .color(Color32::from_gray(145)),
                );
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .id_salt("meetings-list")
                    .show(ui, |ui| {
                        for entry in &self.entries {
                            let selected =
                                matches!(&self.screen, Screen::Meeting(path) if *path == entry.dir);
                            let mark = if entry.transcribed { "  " } else { "* " };
                            let date =
                                meetings::local_time(entry.started_at_unix_ms, "%b %d, %Y  %H:%M");
                            let label = format!(
                                "{mark}{}\n{}  ·  {}  {}",
                                entry.title,
                                date,
                                format_duration(entry.duration_ms),
                                if !matches!(entry.status.as_str(), "completed" | "recording" | "")
                                {
                                    entry.status.as_str()
                                } else {
                                    ""
                                }
                            );
                            let button = egui::Button::new(RichText::new(label).size(12.5))
                                .min_size(Vec2::new(ui.available_width(), 61.0));
                            if ui
                                .add_sized(
                                    [ui.available_width(), 61.0],
                                    button.fill(if selected {
                                        Color32::from_rgb(47, 58, 68)
                                    } else {
                                        Color32::TRANSPARENT
                                    }),
                                )
                                .clicked()
                            {
                                picked = Some(entry.dir.clone());
                            }
                        }
                        if self.entries.is_empty() {
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new("Your recordings will appear here.")
                                    .color(Color32::GRAY)
                                    .size(12.0),
                            );
                        }
                    });
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    if ui
                        .add_sized(
                            [ui.available_width(), 38.0],
                            egui::Button::new("⚙  Settings"),
                        )
                        .clicked()
                    {
                        open_settings = true;
                    }
                    if self.job.is_some() {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("◌  Working…").color(Color32::from_rgb(130, 194, 187)),
                        );
                    }
                });
            });
        if refresh {
            self.refresh_meetings();
        }
        if new_recording {
            self.arm_recording();
        }
        if let Some(dir) = picked {
            self.select_meeting(dir);
        }
        if open_settings {
            self.load_settings_draft();
            self.screen = Screen::Settings;
        }
    }

    fn load_settings_draft(&mut self) {
        self.settings_key = self.config.elevenlabs_api_key.clone().unwrap_or_default();
        self.settings_name = self.config.your_name.clone().unwrap_or_default();
        self.settings_meetings_dir = self.config.meetings_dir().display().to_string();
        self.settings_diarize = self.config.diarize();
        self.settings_auto_transcribe = self.config.auto_transcribe();
        self.settings_message = None;
    }

    fn show_recording(&mut self, ui: &mut egui::Ui) {
        ui.add_space(26.0);
        ui.heading("New recording");
        ui.add_space(6.0);
        let (status, color) = if self.recorder.is_some() {
            ("●  RECORDING", Color32::from_rgb(235, 105, 95))
        } else if self.starting.is_some() {
            ("◌  Starting…", Color32::from_rgb(230, 190, 110))
        } else if self.state == RecordingState::Finalizing {
            ("◌  Saving…", Color32::from_rgb(130, 194, 187))
        } else {
            (
                "Ready, not recording. Check the levels, then press Record.",
                Color32::from_rgb(150, 200, 160),
            )
        };
        ui.label(RichText::new(status).strong().size(15.0).color(color));
        ui.add_space(4.0);
        ui.label(
            RichText::new("Your microphone and the computer's audio are saved as separate tracks.")
                .color(Color32::GRAY),
        );
        ui.add_space(22.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Audio devices").strong());
            if ui.small_button("Refresh devices").clicked() {
                self.refresh_devices();
            }
        });
        ui.add_space(7.0);
        self.device_picker(ui, Side::Mic);
        self.device_picker(ui, Side::Computer);
        ui.add_space(14.0);
        let active = self.recorder.is_some();
        ui.vertical_centered(|ui| {
            let finalizing = matches!(
                self.state,
                RecordingState::Starting | RecordingState::Finalizing
            ) || self.shutting_down;
            let label = if active {
                "Stop recording"
            } else if self.starting.is_some() {
                "Starting…"
            } else if finalizing {
                "Finalizing…"
            } else {
                "Record"
            };
            let color = if active {
                Color32::from_rgb(145, 57, 56)
            } else {
                Color32::from_rgb(46, 112, 93)
            };
            if ui
                .add_enabled(
                    !finalizing,
                    egui::Button::new(RichText::new(label).size(21.0).strong())
                        .fill(color)
                        .min_size(Vec2::new(230.0, 70.0)),
                )
                .clicked()
            {
                if active {
                    self.stop_recording(false);
                } else {
                    self.start_recording();
                }
            }
            ui.add_space(18.0);
            // Fixed-size cell so the timer never nudges the layout as digits change.
            ui.add_sized(
                [230.0, 40.0],
                egui::Label::new(
                    RichText::new(format_duration(self.recording_elapsed.as_millis() as i64))
                        .monospace()
                        .size(31.0),
                )
                .wrap_mode(egui::TextWrapMode::Extend),
            );
        });
        ui.add_space(34.0);
        if let Some(recorder) = &self.recorder {
            ui.label(RichText::new(format!("Microphone  ·  {}", recorder.mic_device())).strong());
            ui.label(
                RichText::new(format!(
                    "Output loopback  ·  {}",
                    recorder.computer_device()
                ))
                .strong(),
            );
        }
        for error in &self.recording_errors {
            ui.add_space(10.0);
            ui.label(RichText::new(error).color(Color32::LIGHT_RED));
        }
        if let Some(notice) = self.notice.clone() {
            ui.add_space(18.0);
            self.notice_box(ui, &notice);
        }
        if self.job.is_some() {
            ui.add_space(28.0);
            self.show_job(ui);
        }
    }

    fn device_picker(&mut self, ui: &mut egui::Ui, side: Side) {
        let index = if side == Side::Mic { 0 } else { 1 };
        let pinned = if index == 0 {
            self.config.mic_device()
        } else {
            self.config.output_device()
        };
        let list = if index == 0 {
            self.input_devices.clone()
        } else {
            self.output_devices.clone()
        };
        let default_name = list
            .iter()
            .find(|device| device.is_default)
            .map(|device| device.name.clone())
            .unwrap_or_else(|| {
                if self.input_devices.is_empty() && self.output_devices.is_empty() {
                    "scanning…".into()
                } else {
                    "unavailable".into()
                }
            });
        let chosen = pinned
            .as_ref()
            .and_then(|id| list.iter().find(|device| &device.id == id));
        let selected = match (pinned.as_ref(), chosen) {
            (None, _) => format!("Follow Windows default ({default_name})"),
            (Some(_), Some(device)) => device.name.clone(),
            (Some(_), None) => format!(
                "{} (unavailable, using default)",
                pinned.as_deref().unwrap_or_default()
            ),
        };
        let title = if index == 0 {
            "Microphone (you)"
        } else {
            "System audio (them)"
        };
        let id_salt = if index == 0 {
            "mic-device-picker"
        } else {
            "output-device-picker"
        };
        let was_open = self.device_popups_open[index];
        let mut new_choice: Option<Option<String>> = None;
        let enabled = self.state == RecordingState::Ready;
        let response = ui
            .horizontal(|ui| {
                ui.add_sized([110.0, 20.0], egui::Label::new(title));
                let combo_width = (ui.available_width() - 10.0).clamp(180.0, 420.0);
                let response = ui
                    .add_enabled_ui(enabled, |ui| {
                        egui::ComboBox::from_id_salt(id_salt)
                            .width(combo_width)
                            .truncate()
                            .selected_text(selected)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(
                                        pinned.is_none(),
                                        format!("Follow Windows default ({default_name})"),
                                    )
                                    .clicked()
                                {
                                    new_choice = Some(None);
                                }
                                if pinned.is_some() && chosen.is_none() {
                                    ui.label(format!(
                                        "{} (unavailable, using default)",
                                        pinned.as_deref().unwrap_or_default()
                                    ));
                                }
                                for device in &list {
                                    if ui
                                        .selectable_label(
                                            pinned.as_ref() == Some(&device.id),
                                            &device.name,
                                        )
                                        .clicked()
                                    {
                                        new_choice = Some(Some(device.id.clone()));
                                    }
                                }
                            })
                    })
                    .inner;
                response
            })
            .inner;
        let level = if self.recorder.is_some() {
            self.recording_meters[index]
        } else {
            self.idle_meters[index]
        };
        let listening = matches!(
            self.state,
            RecordingState::Ready | RecordingState::Recording
        );
        ui.horizontal(|ui| {
            ui.add_space(118.0);
            meter(ui, level, listening, index == 1);
        });
        let is_open = response.inner.is_some();
        self.device_popups_open[index] = is_open;
        if is_open && !was_open {
            self.refresh_devices();
        }
        if let Some(choice) = new_choice {
            let old = pinned;
            if index == 0 {
                self.config.mic_device = choice;
            } else {
                self.config.output_device = choice;
            }
            match self.core.update_config(self.config.clone()) {
                Ok(()) => (),
                Err(error) => {
                    if index == 0 {
                        self.config.mic_device = old;
                    } else {
                        self.config.output_device = old;
                    }
                    self.notice = Some(format!("Could not save audio device: {error}"));
                }
            }
        }
    }

    fn show_meeting(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, dir: PathBuf) {
        if self
            .meeting_cache
            .as_ref()
            .is_none_or(|(path, _)| *path != dir)
        {
            let meeting = meetings::load(&dir).unwrap_or_else(|| {
                let entry = meetings::entry(&dir);
                Meeting {
                    title: entry.as_ref().map(|e| e.title.clone()).unwrap_or_default(),
                    started_at_unix_ms: entry
                        .as_ref()
                        .map(|e| e.started_at_unix_ms)
                        .unwrap_or_default(),
                    duration_ms: entry.as_ref().map(|e| e.duration_ms).unwrap_or_default(),
                    ..Meeting::default()
                }
            });
            self.saved_title = meeting.title.clone();
            self.meeting_dirty = false;
            self.meeting_cache = Some((dir.clone(), meeting));
        }
        let Some((_, mut meeting)) = self.meeting_cache.clone() else {
            return;
        };
        if self.player_dir.as_ref() != Some(&dir) {
            self.player_dir = Some(dir.clone());
            self.player = Some(Player::new(&dir));
        }

        ui.add_space(20.0);
        let mut title_dirty = false;
        let mut title_commit = false;
        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut meeting.title)
                    .id(egui::Id::new("meeting-title"))
                    .font(FontId::proportional(25.0))
                    .desired_width((ui.available_width() - 250.0).max(220.0))
                    .hint_text("Meeting title"),
            );
            title_dirty = response.changed();
            title_commit = response.lost_focus()
                || (response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if ui.button("Copy transcript").clicked() {
                ctx.copy_text(crate::transcript::to_markdown(&meeting));
                self.notice = Some("Transcript copied as Markdown.".into());
            }
            if ui.button("Open folder").clicked() {
                if let Err(error) = Command::new("explorer.exe").arg(&dir).spawn() {
                    self.notice = Some(format!("Could not open folder: {error}"));
                }
            }
        });
        if let Some(player) = &self.player {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui
                    .add_sized(
                        [96.0, 36.0],
                        egui::Button::new(if player.is_playing() { "Pause" } else { "Play" }),
                    )
                    .clicked()
                {
                    player.toggle();
                }
                let duration = player.duration_ms().max(meeting.duration_ms.max(0) as u64);
                let mut seek = player.position_ms().min(duration) as f64;
                if duration > 0 {
                    let response = ui.add_sized(
                        [ui.available_width() - 125.0, 20.0],
                        egui::Slider::new(&mut seek, 0.0..=duration as f64).show_value(false),
                    );
                    if response.changed() {
                        player.play_from(seek.round() as u64);
                    }
                } else {
                    ui.add_sized(
                        [ui.available_width() - 125.0, 20.0],
                        egui::ProgressBar::new(0.0),
                    );
                }
                ui.label(
                    RichText::new(format!(
                        "{} / {}",
                        format_duration(player.position_ms() as i64),
                        format_duration(duration as i64)
                    ))
                    .monospace()
                    .size(12.0),
                );
            });
            if player.is_loading() {
                ui.label(
                    RichText::new("Loading audio…")
                        .color(Color32::GRAY)
                        .size(12.0),
                );
            } else if player.duration_ms() == 0 {
                let message = player
                    .error()
                    .unwrap_or_else(|| "No playable audio tracks were found.".into());
                ui.label(RichText::new(message).color(Color32::GRAY).size(12.0));
            } else if let Some(error) = player.error() {
                ui.label(RichText::new(error).color(Color32::LIGHT_RED).size(12.0));
            }
        }

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);

        let mut editor_lost_focus = false;
        let mut editor_enter = false;
        if !meeting.speakers.is_empty() {
            ui.label(
                RichText::new("SPEAKERS")
                    .size(11.0)
                    .strong()
                    .color(Color32::from_gray(150)),
            );
            ui.horizontal_wrapped(|ui| {
                for speaker in &mut meeting.speakers {
                    ui.horizontal(|ui| {
                        let response = ui.add_sized(
                            [145.0, 25.0],
                            egui::TextEdit::singleline(&mut speaker.name).hint_text(&speaker.id),
                        );
                        if response.changed() {
                            self.meeting_dirty = true;
                        }
                        if response.lost_focus() {
                            editor_lost_focus = true;
                        }
                        if response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            editor_enter = true;
                        }
                    });
                }
            });
            ui.add_space(10.0);
        }

        let mut clicked_at = None;
        let mut active_rect = None;
        let position = self
            .player
            .as_ref()
            .map(Player::position_ms)
            .unwrap_or_default() as i64;
        egui::ScrollArea::vertical()
            .id_salt("transcript-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if meeting.utterances.is_empty() {
                    ui.add_space(30.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("No transcript yet").size(20.0).strong());
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new("Transcribe this recording to build the conversation.")
                                .color(Color32::GRAY),
                        );
                        ui.add_space(12.0);
                        if ui
                            .add_enabled(
                                true,
                                egui::Button::new(RichText::new("Transcribe").size(20.0).strong())
                                    .min_size(Vec2::new(210.0, 48.0)),
                            )
                            .clicked()
                        {
                            self.start_transcription(dir.clone());
                        }
                    });
                } else {
                    let mut order: Vec<usize> = (0..meeting.utterances.len()).collect();
                    order.sort_by_key(|index| meeting.utterances[*index].start_ms);
                    for index in order {
                        let utterance = &meeting.utterances[index];
                        let active = position >= utterance.start_ms && position < utterance.end_ms;
                        let fill = if active {
                            Color32::from_rgb(39, 53, 62)
                        } else {
                            Color32::TRANSPARENT
                        };
                        let frame = egui::Frame::new()
                            .fill(fill)
                            .inner_margin(egui::Margin::symmetric(9, 7))
                            .corner_radius(egui::CornerRadius::same(5));
                        let response = frame.show(ui, |ui| {
                            ui.horizontal_top(|ui| {
                                ui.label(
                                    RichText::new(format_time(utterance.start_ms))
                                        .monospace()
                                        .size(12.0)
                                        .color(Color32::from_gray(150)),
                                );
                                ui.add_space(8.0);
                                ui.vertical(|ui| {
                                    ui.label(
                                        RichText::new(meeting.speaker_name(&utterance.speaker))
                                            .strong()
                                            .color(speaker_color(
                                                &utterance.speaker,
                                                utterance.side,
                                            )),
                                    );
                                    ui.add(egui::Label::new(&utterance.text).wrap());
                                });
                            });
                        });
                        let hit = ui.interact(
                            response.response.rect,
                            egui::Id::new(("utterance", index)),
                            Sense::click(),
                        );
                        if hit.clicked() {
                            clicked_at = Some(utterance.start_ms.max(0) as u64);
                        }
                        if active {
                            active_rect = Some(response.response.rect);
                        }
                    }
                }
            });
        if let Some(ms) = clicked_at {
            if let Some(player) = &self.player {
                player.play_from(ms);
            }
        }
        if let Some(rect) = active_rect {
            if self.player.as_ref().is_some_and(Player::is_playing) {
                ui.scroll_to_rect(rect, Some(Align::Center));
            }
        }

        if title_dirty {
            self.meeting_dirty = true;
        }
        if self.meeting_dirty
            && !self.core.jobs().get(&dir).is_some_and(|j| {
                matches!(
                    j,
                    crate::core::JobState::Queued
                        | crate::core::JobState::Running
                        | crate::core::JobState::Cancelling
                )
            })
            && self.recorder.is_none()
            && (title_commit || editor_lost_focus || editor_enter)
        {
            match meetings::save(&dir, &meeting) {
                Ok(()) => {
                    let new_dir = if meeting.title.trim() != self.saved_title.trim() {
                        self.core
                            .rename_meeting(dir.clone(), meeting.title.clone())
                            .unwrap_or_else(|error| {
                                self.notice = Some(error);
                                dir.clone()
                            })
                    } else {
                        dir.clone()
                    };
                    self.saved_title = meeting.title.clone();
                    self.meeting_dirty = false;
                    self.refresh_meetings();
                    self.screen = Screen::Meeting(new_dir.clone());
                    if self.player_dir.as_ref() != Some(&new_dir) {
                        self.player_dir = Some(new_dir.clone());
                        self.player = Some(Player::new(&new_dir));
                    }
                    self.meeting_cache = Some((new_dir, meeting.clone()));
                }
                Err(error) => self.notice = Some(error),
            }
        } else {
            self.meeting_cache = Some((dir.clone(), meeting));
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui
                .button(if meetings::entry(&dir).is_some_and(|e| e.transcribed) {
                    "Transcribe again"
                } else {
                    "Transcribe"
                })
                .clicked()
            {
                self.start_transcription(dir.clone());
            }
            if let Some(notice) = self.notice.clone() {
                self.notice_box(ui, &notice);
            }
        });
        if self.job.is_some() {
            ui.add_space(12.0);
            self.show_job(ui);
        }
        if self
            .player
            .as_ref()
            .is_some_and(|p| p.is_loading() || p.is_playing())
        {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        ui.add_space(24.0);
        ui.heading("Settings");
        ui.add_space(20.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.set_max_width(700.0);
            ui.label(RichText::new("ElevenLabs API key").strong());
            ui.horizontal(|ui| {
                ui.add_sized(
                    [460.0, 32.0],
                    egui::TextEdit::singleline(&mut self.settings_key)
                        .password(!self.settings_show_key)
                        .hint_text("Paste your API key"),
                );
                ui.checkbox(&mut self.settings_show_key, "Show");
            });
            ui.add_space(19.0);

            ui.label(RichText::new("Transcription language: Automatic").strong());
            ui.label("ElevenLabs detects the spoken language.");
            ui.checkbox(
                &mut self.settings_auto_transcribe,
                "Transcribe automatically after stopping",
            );
            ui.add_space(19.0);

            ui.label(RichText::new("Your name").strong());
            ui.add_sized(
                [360.0, 32.0],
                egui::TextEdit::singleline(&mut self.settings_name)
                    .hint_text("Used for your microphone speaker"),
            );
            ui.add_space(19.0);

            ui.label(RichText::new("Meetings folder").strong());
            ui.horizontal(|ui| {
                ui.add_sized(
                    [500.0, 32.0],
                    egui::TextEdit::singleline(&mut self.settings_meetings_dir),
                );
                if ui.button("Browse…").clicked() {
                    if self.folder_picker.is_none() {
                        let folder = self.settings_meetings_dir.clone();
                        let (tx, rx) = mpsc::channel();
                        self.folder_picker = Some(rx);
                        thread::spawn(move || {
                            let _ =
                                tx.send(rfd::FileDialog::new().set_directory(folder).pick_folder());
                        });
                    }
                }
            });
            ui.add_space(17.0);
            ui.checkbox(
                &mut self.settings_diarize,
                "Find several speakers per side (Nemotron)",
            );
            ui.add_space(25.0);
            if ui
                .add_sized([130.0, 38.0], egui::Button::new("Save settings"))
                .clicked()
            {
                self.save_settings();
            }
            if let Some(message) = &self.settings_message {
                ui.add_space(10.0);
                let color = if message.starts_with("Saved") {
                    Color32::from_rgb(125, 205, 161)
                } else {
                    Color32::LIGHT_RED
                };
                ui.label(RichText::new(message).color(color));
            }
        });
        if self.job.is_some() {
            ui.add_space(16.0);
            self.show_job(ui);
        }
    }

    fn save_settings(&mut self) {
        self.config.elevenlabs_api_key = nonempty(&self.settings_key);
        self.config.language = None;
        self.config.auto_transcribe = Some(self.settings_auto_transcribe);
        self.config.your_name = nonempty(&self.settings_name);
        let folder = self.settings_meetings_dir.trim();
        self.config.meetings_dir = if folder.is_empty() {
            None
        } else {
            Some(PathBuf::from(folder))
        };
        self.config.diarize = Some(self.settings_diarize);
        match self.core.update_config(self.config.clone()) {
            Ok(()) => {
                self.settings_message = Some("Saved. Changes are ready to use.".into());
                self.refresh_meetings();
            }
            Err(error) => self.settings_message = Some(error),
        }
    }

    fn show_job(&mut self, ui: &mut egui::Ui) {
        for (dir, state) in self.core.jobs() {
            if state == crate::core::JobState::Queued {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "Queued: {}",
                        dir.file_name().unwrap_or_default().to_string_lossy()
                    ));
                    if ui.small_button("Cancel queued job").clicked() {
                        let _ = self.core.cancel_job(dir.clone());
                    }
                });
            }
        }
        let Some(job) = &mut self.job else { return };
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&job.stage).strong());
                if ui
                    .add_enabled(!job.finalizing, egui::Button::new("Cancel").small())
                    .clicked()
                {
                    let _ = self.core.cancel_job(job.dir.clone());
                    job.stage = "Cancelling…".into();
                }
            });
            ui.add(egui::ProgressBar::new(job.progress as f32).show_percentage());
            if !job.log.is_empty() {
                egui::ScrollArea::vertical()
                    .max_height(100.0)
                    .show(ui, |ui| {
                        for line in &job.log {
                            ui.label(
                                RichText::new(line)
                                    .monospace()
                                    .size(11.0)
                                    .color(Color32::GRAY),
                            );
                        }
                    });
            }
        });
    }

    fn notice_box(&mut self, ui: &mut egui::Ui, message: &str) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(message).color(Color32::from_rgb(235, 190, 122)));
            if message.to_lowercase().contains("settings")
                || message.to_lowercase().contains("api key")
            {
                if ui.link("Open Settings").clicked() {
                    self.load_settings_draft();
                    self.screen = Screen::Settings;
                }
            }
        });
    }

    fn show_close_dialog(&mut self, ctx: &egui::Context) {
        if self.allow_close {
            return;
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !self.shutting_down {
                self.shutting_down = true;
                if let Err(e) = self.core.shutdown() {
                    self.notice = Some(e);
                }
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.show_close_dialog(&ctx);
        self.poll_background(&ctx);
        if let Some(seconds) = self.test_seconds {
            if !self.test_started {
                self.test_started = true;
                self.start_recording();
            } else if self
                .recorder
                .as_ref()
                .is_some_and(|r| r.elapsed().as_secs() >= seconds)
            {
                self.stop_recording(true);
            } else if self.starting.is_none() && self.recorder.is_none() && self.job.is_none() {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        self.show_sidebar(ui);
        egui::CentralPanel::default().show(ui, |ui| match self.screen.clone() {
            Screen::Recording => self.show_recording(ui),
            Screen::Meeting(dir) => self.show_meeting(ui, &ctx, dir),
            Screen::Settings => self.show_settings(ui),
        });
        ctx.request_repaint_after(Duration::from_millis(80));
        if self.folder_picker.is_some()
            || self.starting.is_some()
            || self.test_seconds.is_some()
            || self.recorder.is_some()
            || self.job.is_some()
            || self
                .player
                .as_ref()
                .is_some_and(|p| p.is_playing() || p.is_loading())
        {
            ctx.request_repaint_after(Duration::from_millis(90));
        }
    }
}

/// Level meter on a dBFS scale (-60..0), so ordinary speech around -30 dBFS
/// fills half the bar instead of a few percent, plus a plain-language status.
fn meter(ui: &mut egui::Ui, value: f32, listening: bool, system_audio: bool) {
    let value = value.clamp(0.0, 1.0);
    let dbfs = if value > 0.0 {
        20.0 * value.log10()
    } else {
        f32::NEG_INFINITY
    };
    let has_signal = listening && dbfs.is_finite() && dbfs > -60.0;
    let fraction = if has_signal {
        ((dbfs + 60.0) / 60.0).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 100.0).clamp(120.0, 380.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 16.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 3.0, Color32::from_gray(38));
        if fraction > 0.0 {
            let mut fill = rect;
            fill.set_width(rect.width() * fraction);
            let color = if dbfs > -3.0 {
                Color32::from_rgb(220, 80, 70)
            } else if dbfs > -12.0 {
                Color32::from_rgb(225, 180, 70)
            } else {
                Color32::from_rgb(80, 190, 120)
            };
            ui.painter().rect_filled(fill, 3.0, color);
        }
        // Tick marks every 12 dB help judge loudness at a glance.
        for db in [-48.0_f32, -36.0, -24.0, -12.0] {
            let x = rect.left() + rect.width() * (db + 60.0) / 60.0;
            ui.painter().line_segment(
                [
                    egui::pos2(x, rect.bottom() - 4.0),
                    egui::pos2(x, rect.bottom()),
                ],
                egui::Stroke::new(1.0, Color32::from_gray(90)),
            );
        }
        let (text, color) = if !listening {
            ("not listening".to_owned(), Color32::GRAY)
        } else if has_signal {
            (format!("{dbfs:>4.0} dBFS"), Color32::LIGHT_GRAY)
        } else if system_audio {
            ("silence".to_owned(), Color32::GRAY)
        } else {
            ("silence".to_owned(), Color32::from_rgb(230, 160, 90))
        };
        // Fixed-size, non-wrapping status cell: changing text must not change
        // the row height, or everything below (Stop button, timer) jumps.
        ui.add_sized(
            [90.0, 18.0],
            egui::Label::new(RichText::new(text).color(color).monospace())
                .wrap_mode(egui::TextWrapMode::Truncate),
        );
    });
}

fn smooth_level(old: f32, target: f32) -> f32 {
    if target > old {
        old + (target - old) * 0.72
    } else {
        (old * 0.86).max(target)
    }
}

fn format_duration(ms: i64) -> String {
    let total = (ms.max(0) / 1000) as u64;
    let hours = total / 3600;
    let minutes = total / 60 % 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn format_time(ms: i64) -> String {
    format!("{}", format_duration(ms))
}

fn speaker_color(id: &str, side: Side) -> Color32 {
    const LOCAL: [Color32; 6] = [
        Color32::from_rgb(237, 167, 105),
        Color32::from_rgb(224, 123, 102),
        Color32::from_rgb(220, 190, 110),
        Color32::from_rgb(202, 140, 174),
        Color32::from_rgb(220, 146, 118),
        Color32::from_rgb(190, 177, 120),
    ];
    const REMOTE: [Color32; 6] = [
        Color32::from_rgb(112, 190, 190),
        Color32::from_rgb(126, 167, 224),
        Color32::from_rgb(163, 152, 224),
        Color32::from_rgb(103, 184, 157),
        Color32::from_rgb(117, 176, 209),
        Color32::from_rgb(165, 192, 128),
    ];
    let index = id.bytes().fold(0usize, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(byte as usize)
    }) % 6;
    match side {
        Side::Mic => LOCAL[index],
        Side::Computer => REMOTE[index],
    }
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}
