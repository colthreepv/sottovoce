//! Names a meeting after the application that produced the most system audio.
//!
//! A sampler runs on the core's MTA capture thread while recording. About once
//! a second it enumerates the Core Audio sessions on the recorded render
//! endpoint (the default output, or the pinned output device) and integrates
//! each session's peak meter per application. At stop it returns the friendly
//! name of the app with the most accumulated loudness, ignoring our own process
//! and the Windows "System Sounds" session.
//!
//! Ranking and name resolution are pure and unit-tested; only the enumeration
//! and the PID-to-name lookup touch Windows APIs.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Peaks at or below this are treated as silence and add no energy.
const SILENCE: f32 = 0.005;
/// Minimum integrated energy (peak squared times seconds) before an app is
/// suggested. It filters one-off notification blips; see docs/research.
const MIN_ENERGY: f64 = 0.05;

// ---------------------------------------------------------------------------
// Pure ranking
// ---------------------------------------------------------------------------

/// Integrated loudness per application name.
#[derive(Debug, Default)]
pub struct Ranking {
    energy: HashMap<String, f64>,
}

impl Ranking {
    /// Adds one peak sample. Samples below [SILENCE] contribute nothing, so an
    /// app that merely holds a silent session never gains energy.
    pub fn observe(&mut self, app: &str, peak: f32, dt: Duration) {
        if !(peak > SILENCE) {
            return;
        }
        *self.energy.entry(app.to_owned()).or_insert(0.0) +=
            (peak as f64).powi(2) * dt.as_secs_f64();
    }

    /// The loudest app at or above [MIN_ENERGY]; ties break by name so the
    /// result is deterministic.
    pub fn top(&self) -> Option<&str> {
        self.energy
            .iter()
            .filter(|(_, energy)| **energy >= MIN_ENERGY)
            .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(name, _)| name.as_str())
    }

    #[cfg(test)]
    fn energy(&self, app: &str) -> f64 {
        self.energy.get(app).copied().unwrap_or(0.0)
    }
}

// ---------------------------------------------------------------------------
// Pure name resolution
// ---------------------------------------------------------------------------

/// Common calling apps whose executable name is not a readable label. Only
/// used when neither the MSIX display name nor the file description is useful;
/// browsers keep whatever name Windows reports for them.
pub fn friendly_from_exe(exe_stem: &str) -> Option<&'static str> {
    let lower = exe_stem.trim().to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    match stem {
        "ms-teams" | "teams" => Some("Teams"),
        "whatsapp" => Some("WhatsApp"),
        "telegram" | "telegramdesktop" => Some("Telegram"),
        "slack" => Some("Slack"),
        "zoom" => Some("Zoom"),
        "discord" => Some("Discord"),
        _ => None,
    }
}

/// Turns the pieces gathered from the process into the name shown to the user:
/// the MSIX display name, else the executable file description, else a mapping
/// for known callers, else the executable stem.
pub fn resolve_name(
    exe_stem: &str,
    file_description: Option<&str>,
    package_display: Option<&str>,
) -> String {
    if let Some(package) = non_empty(package_display) {
        return package.to_owned();
    }
    if let Some(description) = non_empty(file_description) {
        // A description that merely repeats the executable name ("ms-teams"
        // for ms-teams.exe) is no friendlier than the map below. A different
        // spelling ("Firefox" for firefox.exe) is, so compare exactly rather
        // than case-insensitively and keep the real product name.
        if description != exe_stem {
            return description.to_owned();
        }
    }
    friendly_from_exe(exe_stem)
        .map(str::to_owned)
        .unwrap_or_else(|| exe_stem.to_owned())
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

// ---------------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------------

/// Samples the recorded render endpoint while recording.
pub struct Sampler {
    ranking: Ranking,
    /// Resolved once per PID: a process is named while it is first seen.
    names: HashMap<u32, Option<String>>,
    last: Option<Instant>,
}

impl Sampler {
    pub fn new() -> Self {
        platform::ensure_com();
        Self {
            ranking: Ranking::default(),
            names: HashMap::new(),
            last: None,
        }
    }

    /// Samples once. output_device is the pinned endpoint id from the config,
    /// or None to follow the current default render endpoint. The first call
    /// only records the timestamp; energy is integrated from the interval
    /// between calls, so call it at a steady cadence (about once a second).
    pub fn sample(&mut self, output_device: Option<&str>, now: Instant) {
        let dt = match self.last.replace(now) {
            Some(previous) => now.saturating_duration_since(previous),
            None => Duration::ZERO,
        };
        if dt.is_zero() {
            return;
        }
        let me = std::process::id();
        for session in platform::sessions(output_device) {
            if session.is_system || session.pid == 0 || session.pid == me {
                continue;
            }
            let name = self
                .names
                .entry(session.pid)
                .or_insert_with(|| platform::app_name(session.pid))
                .clone();
            if let Some(name) = name {
                self.ranking.observe(&name, session.peak, dt);
            }
        }
    }

    /// The suggested app name, if one cleared the energy threshold.
    pub fn top(&self) -> Option<String> {
        self.ranking.top().map(str::to_owned)
    }
}

/// One live session, for the debug CLI. Reading meters plays nothing.
pub struct SessionInfo {
    pub pid: u32,
    pub is_system: bool,
    pub peak: f32,
    pub app: Option<String>,
}

/// Current sessions on the endpoint with their instantaneous peaks.
pub fn snapshot(output_device: Option<&str>) -> Vec<SessionInfo> {
    let me = std::process::id();
    platform::sessions(output_device)
        .into_iter()
        .map(|session| SessionInfo {
            pid: session.pid,
            is_system: session.is_system,
            peak: session.peak,
            app: if session.is_system || session.pid == 0 || session.pid == me {
                None
            } else {
                platform::app_name(session.pid)
            },
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, S_OK};
    use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
        IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
    };
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    use windows::Win32::Storage::Packaging::Appx::{GetPackageFullName, GetPackagePathByFullName};
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::{Interface, PCWSTR, PWSTR};

    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

    pub struct Raw {
        pub pid: u32,
        pub is_system: bool,
        pub peak: f32,
    }

    /// The capture thread is already MTA; this is a defensive no-op when so.
    pub fn ensure_com() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
    }

    /// Every session on the recorded render endpoint. Never fails: a missing
    /// endpoint or an empty session list just yields nothing.
    pub fn sessions(output_device: Option<&str>) -> Vec<Raw> {
        unsafe {
            let Ok(enumerator) =
                CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
            else {
                return Vec::new();
            };
            let Some(device) = endpoint(&enumerator, output_device) else {
                return Vec::new();
            };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                return Vec::new();
            };
            let Ok(list) = manager.GetSessionEnumerator() else {
                return Vec::new();
            };
            let count = list.GetCount().unwrap_or(0);
            let mut out = Vec::with_capacity(count.max(0) as usize);
            for index in 0..count {
                let Ok(control) = list.GetSession(index) else {
                    continue;
                };
                let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let pid = control2.GetProcessId().unwrap_or(0);
                let is_system = control2.IsSystemSoundsSession() == S_OK;
                let peak = control
                    .cast::<IAudioMeterInformation>()
                    .ok()
                    .and_then(|meter| meter.GetPeakValue().ok())
                    .unwrap_or(0.0);
                out.push(Raw {
                    pid,
                    is_system,
                    peak,
                });
            }
            out
        }
    }

    /// The pinned endpoint by id, or the default render endpoint when the id is
    /// absent or no longer present.
    fn endpoint(enumerator: &IMMDeviceEnumerator, pinned: Option<&str>) -> Option<IMMDevice> {
        unsafe {
            if let Some(id) = pinned.filter(|id| !id.is_empty()) {
                if let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                {
                    if let Ok(count) = collection.GetCount() {
                        for index in 0..count {
                            if let Ok(device) = collection.Item(index) {
                                let found = device.GetId().ok().and_then(|id| id.to_string().ok());
                                if found.as_deref() == Some(id) {
                                    return Some(device);
                                }
                            }
                        }
                    }
                }
            }
            enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()
        }
    }

    /// Friendly name for a PID, or None when the process cannot be inspected
    /// (protected or already gone).
    pub fn app_name(pid: u32) -> Option<String> {
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return None;
            };
            let image = process_image(handle);
            let package = package_display_name(handle);
            let _ = CloseHandle(handle);
            let stem = image.as_deref().map(file_stem)?;
            if stem.is_empty() {
                return None;
            }
            let description = image
                .as_deref()
                .and_then(|path| version_string(path, "FileDescription"));
            Some(super::resolve_name(
                &stem,
                description.as_deref(),
                package.as_deref(),
            ))
        }
    }

    fn file_stem(path: &str) -> String {
        std::path::Path::new(path)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn process_image(handle: HANDLE) -> Option<String> {
        unsafe {
            let mut buffer = vec![0u16; 4096];
            let mut length = buffer.len() as u32;
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
            .ok()?;
            if length == 0 {
                return None;
            }
            Some(String::from_utf16_lossy(&buffer[..length as usize]))
        }
    }

    /// MSIX display name from the package manifest, when the process is
    /// packaged and the manifest holds a literal (not an ms-resource token).
    fn package_display_name(handle: HANDLE) -> Option<String> {
        unsafe {
            let mut length = 0u32;
            let error = GetPackageFullName(handle, &mut length, None);
            if error.0 != ERROR_INSUFFICIENT_BUFFER || length == 0 {
                return None;
            }
            let mut buffer = vec![0u16; length as usize];
            let error = GetPackageFullName(handle, &mut length, Some(PWSTR(buffer.as_mut_ptr())));
            if error.0 != 0 {
                return None;
            }
            let full_name = wide_string(&buffer);
            let path = package_path(&full_name)?;
            manifest_display_name(&path)
        }
    }

    fn package_path(full_name: &str) -> Option<String> {
        unsafe {
            let full = wide(full_name);
            let mut length = 0u32;
            let error = GetPackagePathByFullName(PCWSTR(full.as_ptr()), &mut length, None);
            if error.0 != ERROR_INSUFFICIENT_BUFFER || length == 0 {
                return None;
            }
            let mut buffer = vec![0u16; length as usize];
            let error = GetPackagePathByFullName(
                PCWSTR(full.as_ptr()),
                &mut length,
                Some(PWSTR(buffer.as_mut_ptr())),
            );
            if error.0 != 0 {
                return None;
            }
            Some(wide_string(&buffer))
        }
    }

    fn manifest_display_name(package_dir: &str) -> Option<String> {
        let text =
            std::fs::read_to_string(std::path::Path::new(package_dir).join("AppxManifest.xml"))
                .ok()?;
        if let Some(start) = text.find("<DisplayName>") {
            let rest = &text[start + "<DisplayName>".len()..];
            if let Some(end) = rest.find("</DisplayName>") {
                let value = rest[..end].trim();
                if !value.is_empty() && !value.starts_with("ms-resource:") {
                    return Some(value.to_owned());
                }
            }
        }
        if let Some(index) = text.find("DisplayName=\"") {
            let rest = &text[index + "DisplayName=\"".len()..];
            if let Some(end) = rest.find('"') {
                let value = &rest[..end];
                if !value.is_empty() && !value.starts_with("ms-resource:") {
                    return Some(value.to_owned());
                }
            }
        }
        None
    }

    fn version_string(path: &str, key: &str) -> Option<String> {
        unsafe {
            let path_w = wide(path);
            let size = GetFileVersionInfoSizeW(PCWSTR(path_w.as_ptr()), None);
            if size == 0 {
                return None;
            }
            let mut data = vec![0u8; size as usize];
            GetFileVersionInfoW(
                PCWSTR(path_w.as_ptr()),
                None,
                size,
                data.as_mut_ptr() as *mut core::ffi::c_void,
            )
            .ok()?;

            let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut length = 0u32;
            let translation = wide("\\VarFileInfo\\Translation");
            if !VerQueryValueW(
                data.as_ptr() as *const core::ffi::c_void,
                PCWSTR(translation.as_ptr()),
                &mut pointer,
                &mut length,
            )
            .as_bool()
                || length < 4
            {
                return None;
            }
            let language = *(pointer as *const u16);
            let codepage = *((pointer as *const u16).add(1));

            let subblock = wide(&format!(
                "\\StringFileInfo\\{language:04x}{codepage:04x}\\{key}"
            ));
            let mut value: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut value_length = 0u32;
            if !VerQueryValueW(
                data.as_ptr() as *const core::ffi::c_void,
                PCWSTR(subblock.as_ptr()),
                &mut value,
                &mut value_length,
            )
            .as_bool()
                || value_length == 0
            {
                return None;
            }
            let slice = std::slice::from_raw_parts(value as *const u16, value_length as usize);
            let text = String::from_utf16_lossy(slice)
                .trim_end_matches('\0')
                .trim()
                .to_owned();
            if text.is_empty() { None } else { Some(text) }
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn wide_string(buffer: &[u16]) -> String {
        let end = buffer
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    }
}

#[cfg(not(windows))]
mod platform {
    #[allow(dead_code)]
    pub struct Raw {
        pub pid: u32,
        pub is_system: bool,
        pub peak: f32,
    }
    pub fn ensure_com() {}
    pub fn sessions(_output_device: Option<&str>) -> Vec<Raw> {
        Vec::new()
    }
    pub fn app_name(_pid: u32) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn ranking_picks_the_loudest_app() {
        let mut ranking = Ranking::default();
        ranking.observe("Teams", 0.2, seconds(3));
        ranking.observe("Slack", 0.05, seconds(3));
        assert_eq!(ranking.top(), Some("Teams"));
        assert!(ranking.energy("Teams") > ranking.energy("Slack"));
    }

    #[test]
    fn ranking_ignores_near_silence() {
        let mut ranking = Ranking::default();
        for _ in 0..600 {
            ranking.observe("Chrome", 0.001, seconds(1));
        }
        assert_eq!(ranking.energy("Chrome"), 0.0);
        assert_eq!(ranking.top(), None);
    }

    #[test]
    fn ranking_requires_meaningful_energy() {
        let mut ranking = Ranking::default();
        ranking.observe("Discord", 0.1, seconds(1));
        assert_eq!(ranking.top(), None, "a flick should not name a meeting");
        ranking.observe("Discord", 0.2, seconds(10));
        assert_eq!(ranking.top(), Some("Discord"));
    }

    #[test]
    fn ranking_breaks_ties_by_name() {
        let mut ranking = Ranking::default();
        ranking.observe("Zoom", 0.3, seconds(2));
        ranking.observe("Slack", 0.3, seconds(2));
        assert_eq!(ranking.top(), Some("Slack"));
    }

    #[test]
    fn resolve_prefers_package_display_name() {
        assert_eq!(
            resolve_name("WhatsApp", Some("WhatsApp"), Some("WhatsApp Desktop")),
            "WhatsApp Desktop"
        );
    }

    #[test]
    fn resolve_uses_file_description_when_available() {
        assert_eq!(
            resolve_name("ms-teams", Some("Microsoft Teams"), None),
            "Microsoft Teams"
        );
    }

    #[test]
    fn resolve_maps_known_callers() {
        for (stem, expected) in [
            ("ms-teams", "Teams"),
            ("teams", "Teams"),
            ("whatsapp", "WhatsApp"),
            ("Telegram", "Telegram"),
            ("slack", "Slack"),
            ("Zoom", "Zoom"),
            ("Discord", "Discord"),
        ] {
            assert_eq!(resolve_name(stem, None, None), expected, "{stem}");
        }
    }

    #[test]
    fn resolve_maps_when_the_description_is_just_the_stem() {
        assert_eq!(resolve_name("ms-teams", Some("ms-teams"), None), "Teams");
    }

    #[test]
    fn resolve_keeps_a_real_description_that_differs_only_in_case() {
        assert_eq!(resolve_name("firefox", Some("Firefox"), None), "Firefox");
    }

    #[test]
    fn resolve_keeps_browsers_and_unknowns_as_reported() {
        assert_eq!(
            resolve_name("chrome", Some("Google Chrome"), None),
            "Google Chrome"
        );
        assert_eq!(resolve_name("mystery", None, None), "mystery");
        assert_eq!(resolve_name("mystery", Some("  "), None), "mystery");
    }
}
