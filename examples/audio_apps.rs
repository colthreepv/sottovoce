//! Research probe: which application is producing the audio on the default
//! render endpoint?
//!
//! This is not part of the recorder. It opens the default output device (the
//! same endpoint WASAPI loopback records), enumerates the Core Audio sessions
//! on it, and roughly every 250 ms reads each session's peak meter. It maps
//! each session's owning PID to a friendly app name, accumulates the energy
//! over the run and reports the loudest application.
//!
//! Usage:
//!     cargo run --example audio_apps -- [seconds]
//!
//! Seconds default to 10. Nothing is captured or played back; only per-session
//! peak meters are read.

// Hand-written Win32 FFI: keep the probe terse rather than wrap every call.
#![allow(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, S_OK};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    MMDeviceEnumerator,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::Storage::Packaging::Appx::{GetPackageFullName, GetPackagePathByFullName};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// How often every session is sampled.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
/// A tick's session peak below this is treated as silence in the live line.
const SILENCE: f32 = 0.0005;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// A resolved identity for one process.
#[derive(Clone)]
struct AppIdent {
    name: String,
    source: &'static str,
    exe: Option<String>,
    package: Option<String>,
}

/// Everything accumulated for one audio session over the run.
#[derive(Default, Clone)]
struct Agg {
    pid: u32,
    app: String,
    source: String,
    display_name: String,
    exe: String,
    package: Option<String>,
    is_system: bool,
    meter_ok: bool,
    energy: f64,
    peak_max: f32,
    peak_last: f32,
    ticks: u32,
}

fn main() {
    let seconds: f64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(10.0);
    if let Err(e) = run(seconds) {
        eprintln!("error: {e}");
        std::process::exit(2);
    }
}

fn run(seconds: f64) -> Result<(), String> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        // Already initialized with a different model is fine for reading.
        if hr.is_err() && hr.0 != -2147417850 {
            return Err(format!("CoInitializeEx failed: {hr:?}"));
        }

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| e.to_string())?;
        let endpoint_id = device
            .GetId()
            .ok()
            .and_then(|p| p.to_string().ok())
            .unwrap_or_default();
        let manager: IAudioSessionManager2 = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| e.to_string())?;

        println!(
            "Sampling default render endpoint for {seconds:.1}s (every {} ms)",
            SAMPLE_INTERVAL.as_millis()
        );
        println!("endpoint: {endpoint_id}");
        println!();

        let mut cache: HashMap<u32, AppIdent> = HashMap::new();
        let mut aggs: HashMap<String, Agg> = HashMap::new();
        let start = Instant::now();
        let total = Duration::from_secs_f64(seconds);

        while start.elapsed() < total {
            let tick = start.elapsed();
            let enumerator = match manager.GetSessionEnumerator() {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("[{:5.2}s] GetSessionEnumerator failed: {e}", tick.as_secs_f64());
                    std::thread::sleep(SAMPLE_INTERVAL);
                    continue;
                }
            };
            let count = enumerator.GetCount().unwrap_or(0);
            let mut tick_apps: HashMap<String, f32> = HashMap::new();

            for i in 0..count {
                let Ok(ctl) = enumerator.GetSession(i) else {
                    continue;
                };
                let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let pid = ctl2.GetProcessId().unwrap_or(0);
                let is_system = ctl2.IsSystemSoundsSession() == S_OK;
                let display_name = ctl
                    .GetDisplayName()
                    .ok()
                    .and_then(|p| p.to_string().ok())
                    .unwrap_or_default();
                let instance = ctl2
                    .GetSessionInstanceIdentifier()
                    .ok()
                    .and_then(|p| p.to_string().ok())
                    .unwrap_or_default();
                let key = if instance.is_empty() {
                    format!("pid:{pid}:{i}")
                } else {
                    instance
                };
                let (peak, meter_ok) = match ctl.cast::<IAudioMeterInformation>() {
                    Ok(m) => (m.GetPeakValue().unwrap_or(0.0), true),
                    Err(_) => (0.0, false),
                };

                let ident = cache
                    .entry(pid)
                    .or_insert_with(|| resolve_app(pid))
                    .clone();
                let agg = aggs.entry(key).or_insert_with(|| Agg {
                    pid,
                    app: ident.name.clone(),
                    source: ident.source.to_string(),
                    display_name: display_name.clone(),
                    exe: ident.exe.clone().unwrap_or_default(),
                    package: ident.package.clone(),
                    is_system,
                    meter_ok,
                    ..Default::default()
                });
                agg.energy += (peak as f64).powi(2) * SAMPLE_INTERVAL.as_secs_f64();
                agg.peak_last = peak;
                agg.peak_max = agg.peak_max.max(peak);
                agg.ticks += 1;
                agg.meter_ok |= meter_ok;

                if peak > SILENCE {
                    *tick_apps.entry(ident.name.clone()).or_insert(0.0) += peak;
                }
            }

            let t = tick.as_secs_f64();
            if tick_apps.is_empty() {
                println!("[{t:5.2}s] …");
            } else {
                let mut v: Vec<(String, f32)> = tick_apps.into_iter().collect();
                v.sort_by(|a, b| b.1.total_cmp(&a.1));
                let parts: Vec<String> =
                    v.iter().map(|(n, p)| format!("{} {p:.3}", trunc(n, 28))).collect();
                println!("[{t:5.2}s] {}", parts.join("  |  "));
            }
            std::thread::sleep(SAMPLE_INTERVAL);
        }

        print_report(&aggs);
        Ok(())
    }
}

fn print_report(aggs: &HashMap<String, Agg>) {
    let mut rows: Vec<&Agg> = aggs.values().filter(|a| a.ticks > 0).collect();
    rows.sort_by(|a, b| b.energy.total_cmp(&a.energy));

    println!();
    println!("--- sessions ({}) ---", rows.len());
    let meters = rows.iter().filter(|a| a.meter_ok).count();
    println!(
        "peak meter interface obtained on {meters}/{} sessions (0.0 above therefore means silence, not a failed cast)",
        rows.len()
    );
    println!(
        "{:<34} {:>7} {:<14} {:>8} {:>8} {:>10} {:>6} {}",
        "APP", "PID", "SOURCE", "PEAKMAX", "PEAKLAST", "ENERGY", "TICKS", "SESSION DISPLAY"
    );
    for a in &rows {
        let tag = if a.is_system { " [system]" } else { "" };
        println!(
            "{:<34} {:>7} {:<14} {:>8.4} {:>8.4} {:>10.5} {:>6} {}",
            trunc(&format!("{}{}", a.app, tag), 34),
            a.pid,
            a.source,
            a.peak_max,
            a.peak_last,
            a.energy,
            a.ticks,
            trunc(&a.display_name, 28)
        );
    }

    // Group sessions by application name; this is what a browser or an
    // Electron app with several child processes collapses into.
    let packaged: Vec<&Agg> = rows.iter().copied().filter(|a| a.package.is_some()).collect();
    if !packaged.is_empty() {
        println!();
        println!("--- packaged (MSIX) processes ---");
        for a in packaged {
            println!(
                "{:<28} {}   [{}]",
                trunc(&a.app, 28),
                a.package.as_deref().unwrap_or(""),
                a.exe
            );
        }
    }

    let mut by_app: HashMap<String, (f64, f32, Vec<u32>, bool)> = HashMap::new();
    for a in aggs.values() {
        let e = by_app.entry(a.app.clone()).or_insert((0.0, 0.0, vec![], false));
        e.0 += a.energy;
        e.1 = e.1.max(a.peak_max);
        if !e.2.contains(&a.pid) {
            e.2.push(a.pid);
        }
        e.3 |= a.is_system;
    }
    let mut apps: Vec<(String, (f64, f32, Vec<u32>, bool))> = by_app.into_iter().collect();
    apps.sort_by(|a, b| b.1 .0.total_cmp(&a.1 .0));

    println!();
    println!("--- applications ---");
    println!("{:<34} {:>8} {:>10}  {}", "APP", "PEAKMAX", "ENERGY", "PIDS");
    for (name, (energy, peak_max, pids, is_system)) in &apps {
        let tag = if *is_system { " [system]" } else { "" };
        let pids: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
        println!(
            "{:<34} {:>8.4} {:>10.5}  {}",
            trunc(&format!("{name}{tag}"), 34),
            peak_max,
            energy,
            pids.join(",")
        );
    }

    let top = apps
        .iter()
        .filter(|(_, (energy, _, _, is_system))| !*is_system && *energy > 0.0)
        .max_by(|a, b| a.1 .0.total_cmp(&b.1 .0));
    println!();
    match top {
        Some((name, (energy, peak_max, pids, _))) => println!(
            "TOP APP (system sounds excluded): {name}  energy={energy:.5}  peakmax={peak_max:.4}  pids={:?}",
            pids
        ),
        None => println!("TOP APP (system sounds excluded): none — nothing measured above silence"),
    }
}

/// Resolve a PID to a friendly name, opening the process read-only.
fn resolve_app(pid: u32) -> AppIdent {
    if pid == 0 {
        return AppIdent {
            name: "System Sounds".into(),
            source: "well-known",
            exe: None,
            package: None,
        };
    }
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return AppIdent {
                name: format!("PID {pid}"),
                source: "no access",
                exe: None,
                package: None,
            };
        };
        let ident = resolve_handle(pid, handle);
        let _ = CloseHandle(handle);
        ident
    }
}

unsafe fn resolve_handle(pid: u32, handle: HANDLE) -> AppIdent {
    let exe = process_image(handle);
    let package = package_full_name(handle);

    let mut name: Option<String> = None;
    let mut source = "exe";

    // MSIX / Store app: prefer the package's display name from its manifest.
    if let Some(full) = &package {
        if let Some(dir) = package_path(full) {
            if let Some(dn) = manifest_display_name(&dir) {
                name = Some(dn);
                source = "package";
            }
        }
    }

    // Classic Win32 (and packaged exes that still carry usable version info).
    if name.is_none() {
        if let Some(p) = &exe {
            if let Some(fd) = version_string(p, "FileDescription") {
                name = Some(fd);
                source = "FileDescription";
            } else if let Some(pn) = version_string(p, "ProductName") {
                name = Some(pn);
                source = "ProductName";
            }
        }
    }

    if name.is_none() {
        if let Some(p) = &exe {
            name = std::path::Path::new(p)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned());
            source = "exe name";
        }
    }

    AppIdent {
        name: name.unwrap_or_else(|| format!("PID {pid}")),
        source,
        exe,
        package,
    }
}

/// Full image path of a process, read-only.
unsafe fn process_image(handle: HANDLE) -> Option<String> {
    let mut buf = vec![0u16; 4096];
    let mut len = buf.len() as u32;
    QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).ok()?;
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// Package full name if the process is a packaged (MSIX/Store) app.
unsafe fn package_full_name(handle: HANDLE) -> Option<String> {
    let mut len = 0u32;
    let err = GetPackageFullName(handle, &mut len, None);
    if err.0 != ERROR_INSUFFICIENT_BUFFER || len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    let err = GetPackageFullName(handle, &mut len, Some(PWSTR(buf.as_mut_ptr())));
    if err.0 != 0 {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

unsafe fn package_path(full: &str) -> Option<String> {
    let full_w = wide(full);
    let mut len = 0u32;
    let err = GetPackagePathByFullName(PCWSTR(full_w.as_ptr()), &mut len, None);
    if err.0 != ERROR_INSUFFICIENT_BUFFER || len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    let err = GetPackagePathByFullName(
        PCWSTR(full_w.as_ptr()),
        &mut len,
        Some(PWSTR(buf.as_mut_ptr())),
    );
    if err.0 != 0 {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// Best-effort display name from the package's AppxManifest.xml. MSIX
/// resources often store a "ms-resource:..." token, which we skip and leave
/// for the WinRT PackageManager to resolve later.
fn manifest_display_name(pkg_dir: &str) -> Option<String> {
    let text =
        std::fs::read_to_string(std::path::Path::new(pkg_dir).join("AppxManifest.xml")).ok()?;
    if let Some(start) = text.find("<DisplayName>") {
        let rest = &text[start + "<DisplayName>".len()..];
        if let Some(end) = rest.find("</DisplayName>") {
            let v = rest[..end].trim();
            if !v.is_empty() && !v.starts_with("ms-resource:") {
                return Some(v.to_string());
            }
        }
    }
    if let Some(idx) = text.find("DisplayName=\"") {
        let rest = &text[idx + "DisplayName=\"".len()..];
        if let Some(end) = rest.find('"') {
            let v = &rest[..end];
            if !v.is_empty() && !v.starts_with("ms-resource:") {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// A single version-info string (FileDescription/ProductName) from an exe.
unsafe fn version_string(path: &str, key: &str) -> Option<String> {
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
        data.as_mut_ptr() as *mut c_void,
    )
    .ok()?;

    let mut ptr: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    let trans = wide("\\VarFileInfo\\Translation");
    if !VerQueryValueW(
        data.as_ptr() as *const c_void,
        PCWSTR(trans.as_ptr()),
        &mut ptr,
        &mut len,
    )
    .as_bool()
        || len < 4
    {
        return None;
    }
    let lang = *(ptr as *const u16);
    let cp = *((ptr as *const u16).add(1));

    let sub = wide(&format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\{key}"));
    let mut vptr: *mut c_void = std::ptr::null_mut();
    let mut vlen = 0u32;
    if !VerQueryValueW(
        data.as_ptr() as *const c_void,
        PCWSTR(sub.as_ptr()),
        &mut vptr,
        &mut vlen,
    )
    .as_bool()
        || vlen == 0
    {
        return None;
    }
    let slice = std::slice::from_raw_parts(vptr as *const u16, vlen as usize);
    let s: String = String::from_utf16_lossy(slice).trim_end_matches('\0').trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
