# Which application is that system audio from?

Status: Implemented in `src/app_audio.rs` and integrated through `src/core/mod.rs` for app-name meeting title suggestions.

Research + prototype for naming a new meeting after the application that is
actually playing audio, the way the Windows volume mixer lists per-app sliders.

Short answer: **yes, on Windows 10/11 this is available and cheap.** Every audio
client that plays to the render endpoint owns a *Core Audio session*, and each
session exposes the owning process id and a per-session peak meter. We can sample
that during the recording for essentially free and name the meeting after the app
with the most accumulated energy.

Prototype: `examples/audio_apps.rs` (`cargo run --example audio_apps -- 10`).
It samples every ~250 ms, prints per-session and per-app peak/energy, and the top
app at the end. It reads peak meters only — it never captures or plays audio.

## What was verified on this machine

A 10 s run (nothing intentionally playing) enumerated three sessions and resolved
all names:

```
--- sessions (3) ---
APP                                    PID SOURCE          PEAKMAX ...
ChatGPT                              33944 package          0.0000 ...
Firefox                              18624 FileDescription   0.0000 ...
System Sounds [system]                   0                  0.0000 ...

--- packaged (MSIX) processes ---
ChatGPT   OpenAI.Codex_26.928.4866.0_x64__2p2nqsd0c76g0   [C:\Program Files\WindowsApps\OpenAI.Codex_...\app\ChatGPT.exe]
```

A second run happened to catch real output and confirmed the whole chain — peak
meter, energy, ranking:

```
--- sessions ---
peak meter interface obtained on 4/4 sessions (0.0 above therefore means silence, not a failed cast)
APP                                    PID SOURCE          PEAKMAX PEAKLAST     ENERGY
Windows PowerShell                   23316 FileDescription   0.3989   0.0091    0.03980
ChatGPT                              33944 package          0.0000   0.0000    0.00000
Firefox                              18624 FileDescription   0.0000   0.0000    0.00000
System Sounds [system]                   0 well-known       0.0000   0.0000    0.00000

TOP APP (system sounds excluded): Windows PowerShell  energy=0.03980  peakmax=0.3989  pids=[23316]
```

Verified by that run:

- Sessions are enumerable on the default render endpoint.
- `IAudioMeterInformation` is obtainable **per session** and returns real
  peaks (0.399 for a process that emitted sound; 0.0 for the silent ones).
- Friendly names resolve for a classic Win32 process (`Windows PowerShell`,
  `Firefox` — via `FileDescription`) and for a packaged app
  (`ChatGPT` — via its MSIX manifest `DisplayName`).
- The Windows "System Sounds" session is identifiable and can be excluded.

Not verified here (no audio was played on purpose): browser/Electron multi-process
attribution, WebView2, per-channel meters, and `IsSystemSoundsSession` on a
machine with system sounds active. See "Hypotheses to confirm".

## The session APIs

On the render endpoint the recorder already records from — cpal's default output
uses `IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender, eConsole)` in
`cpal-0.18.2/src/host/wasapi/device.rs:1159`, and `IAudioSessionManager2`
attaches to that same `IMMDevice`:

```
IMMDeviceEnumerator (CLSID MMDeviceEnumerator)   [CoCreateInstance]
  └─ GetDefaultAudioEndpoint(eRender, eConsole)         -> IMMDevice
       ├─ GetId()                                        -> endpoint id (PWSTR)
       └─ Activate::<IAudioSessionManager2>(CLSCTX_ALL)  -> IAudioSessionManager2
            └─ GetSessionEnumerator()                    -> IAudioSessionEnumerator
                 ├─ GetCount()                           -> i32
                 └─ GetSession(i)                        -> IAudioSessionControl
                      ├─ GetDisplayName()                -> PWSTR (may be empty)
                      ├─ GetSessionIdentifier()          -> PWSTR
                      ├─ cast::<IAudioSessionControl2>()
                      │    ├─ GetProcessId()             -> u32
                      │    ├─ IsSystemSoundsSession()    -> HRESULT (S_OK == 0 means system)
                      │    └─ GetSessionInstanceIdentifier() -> PWSTR (unique per session)
                      └─ cast::<IAudioMeterInformation>()   [Endpoints]
                           └─ GetPeakValue()             -> f32, linear 0.0..=1.0
```

Mapping to `windows-rs` 0.62 (the version cpal already pins, see Cargo.lock):
`IMMDeviceEnumerator`, `IMMDevice`, `IAudioSessionManager2`,
`IAudioSessionEnumerator`, `IAudioSessionControl`, `IAudioSessionControl2`
are in `windows::Win32::Media::Audio`; `IAudioMeterInformation` is in
`...::Media::Audio::Endpoints`.

### Per-session peak metering

`IAudioMeterInformation` is documented as a *device* interface, but the same
interface is implemented on the audio session object: casting
`IAudioSessionControl` to `IAudioMeterInformation` and calling
`GetPeakValue()` returns that session's instantaneous peak. This is the
technique used by per-app volume/mixer tools. It worked for every session in the
probe (4/4), and the nonzero reading above shows it tracks real audio. Treat it as
a reliable technique, not a formal contract — keep the "meter unavailable" path
(the example tracks whether the cast succeeded) so a failed cast is never
mistaken for silence.

`GetPeakValue` is an *instantaneous linear peak*, not loudness. Sampling it
every 250 ms and accumulating `peak^2 * dt` (energy proxy) ranks "who
dominated the call" much better than the last value or the max. Use
`GetChannelsPeakValues` only if you need per-channel detail; a mono peak is
enough for naming.

## Turning a PID into a friendly name

Resolution order used by the prototype (cache per PID for the whole recording —
processes can exit while the session lingers):

1. `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)`.
   Works for the user's own processes and packaged apps. Fails with access denied
   for protected/elevated processes — fall back to `PID <n>`.
2. `QueryFullProcessImageNameW` → exe path.
3. **Classic Win32:** `GetFileVersionInfoSizeW` / `GetFileVersionInfoW` /
   `VerQueryValueW` on the `\VarFileInfo\Translation` table, then
   `\StringFileInfo\<lang><cp>\FileDescription` (preferred) or
   `ProductName`. This gave `Firefox` and `Windows PowerShell`
   above. Skip generic/empty values.
4. **Packaged (MSIX/Store) apps:** `GetPackageFullName(handle, ..)` succeeds →
   it is a packaged app. `GetPackagePathByFullName` gives the install
   directory; read `AppxManifest.xml` and take `<DisplayName>` (element)
   or `DisplayName="..."` (attribute). WhatsApp desktop, the Store Teams
   client, Spotify, etc. land here. This gave `ChatGPT` above.
   - Many manifests store `ms-resource:...` instead of a literal. The
     prototype skips those and falls back; a production implementation should
     resolve them with the WinRT `PackageManager`
     (`Windows.ApplicationModel.Package.DisplayName`, which resolves MRT
     resources). That is the one piece of this design that needs WinRT rather than
     plain Win32.
   - Fallback if the manifest name is unusable: the exe's `FileDescription`
     inside the package is often still good, else the package `Identity Name`.
5. **Fall back:** exe file stem, then `PID <n>`.

### Browsers and Electron apps (Slack, Teams)

Audio sessions belong to the **process that created the audio session**, not to a
window. Multi-process apps (Chrome/Edge, Slack, Teams, Discord) therefore show up
as one or more sessions that may be owned by renderer/auxiliary/audio-service
child processes rather than the main `Slack.exe` / `ms-teams.exe`:

- Often the session is attributed to the main browser/Electron process; sometimes
  it is a renderer or a dedicated audio process.
- Resolve the PID to a name **per session**, then group by the resolved name in
  the report (the prototype does this in its `--- applications ---` table).
  That merges child processes of the same app into one candidate, which is what we
  want for naming.
- WebView2 hosts (and Tauri in the future) behave like Electron: audio is
  attributed to the WebView2 browser-process tree, so a session may be named
  `msedgewebview2` rather than the host app. Group by name and accept that
  the name is imperfect, or map known process names to product names.

The prototype's `SESSION DISPLAY` column is
`IAudioSessionControl::GetDisplayName` — browsers sometimes put the tab/page
title there. It is optional and often empty; don't depend on it.

## Integration sketch into the recorder

Attribution needs no second capture device and no cpal stream handle: it only
needs the same **endpoint** the computer track records from, so a pinned device
stays consistent. Sottovoce already stores the cpal `DeviceId` string as
`DeviceChoice::Pinned(String)` (src/devices.rs), and cpal's WASAPI
`DeviceId` string equals `IMMDevice::GetId()` (both are the
`{0.0.0.0.00000000}.{guid}` endpoint id — the probe printed exactly that
format). So the integration can match the pinned device by id, or fall back to the
default render endpoint.

````mermaid
flowchart LR
    A[record start] --> B[spawn attribution thread<br/>or reuse supervisor]
    B --> C{every 250 ms}
    C --> D[GetSessionEnumerator<br/>on the recorded endpoint]
    D --> E[per session: PID, IsSystemSounds,<br/>cast to IAudioMeterInformation, GetPeakValue]
    E --> F[energy += peak^2 * dt<br/>keyed by resolved app name]
    F --> C
    A --> G[record stop]
    G --> H[pick max-energy app<br/>excluding System Sounds + own PID]
    H --> I[suggested name, else "Meeting"]
````

Concrete shape:

- Start a small sampler (or a sub-loop of the existing capture supervisor) when
  recording starts, stop it on stop. COM should be initialized on that thread with
  `CoInitializeEx(COINIT_MULTITHREADED)`.
- Accumulate `HashMap<app_name, Energy>` plus first/last-seen and max peak.
- On stop, exclude:
  - sessions where `IsSystemSoundsSession() == S_OK` (pid 0 / System Sounds),
  - the recorder's own PID (transcript playback would otherwise win),
  - names that failed to resolve.
- Require a minimum accumulated energy before suggesting a name; otherwise fall
  back to the current `Meeting` default. That avoids naming after a one-off
  notification blip.
- Store the suggestion next to the session metadata. `capture::Session`
  already serializes to `session.json`; a new optional field such as
  `suggested_app: Option<String>` (plus the runner-up, for a UI dropdown)
  fits that struct without touching the audio path. Attribution is advisory: the
  user can always rename.

## Future option: process loopback capture

Windows 10 2004+ (build 19041) supports
`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` via
`ActivateAudioInterfaceAsync` with the
`VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK` device and
`AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId, ProcessLoopbackMode }`
(`PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`). It records only a
chosen process tree instead of the whole endpoint.

That is a *capture* capability, not an attribution one: once attribution picks the
meeting app, process loopback could record just it (cleaner audio, no notification
dings). It does not remove the need for the session-meter work above — you still
have to decide *which* PID to capture. It is also more disruptive: it replaces the
current cpal loopback path and only covers audio that process renders, so hold it
for a later phase.

## Risks and limitations

- **Instantaneous peak, not loudness.** A short loud burst can outrank steady
  speech. Accumulated `peak^2` over 250 ms ticks mitigates this. Tune the
  tick and the minimum-energy threshold against real calls.
- **Session lifetime.** A session exists only while the app has an audio client. An
  app that stays silent for the whole call may never appear (or appear late);
  sampling from the start of the recording catches apps that play first. Some apps
  keep an idle session alive for a long time, so "present" is not "playing".
- **Attribution is per session, not per window.** Multiple tabs/meetings in one
  browser merge into one name. That is usually the desired answer ("Chrome"), but
  it cannot say *which* tab.
- **Helper/child processes.** Electron and browsers may attribute audio to
  `msedgewebview2`, `crashpad`, an "Audio Service", etc. Grouping by
  resolved name helps; a small mapping table for known process names makes it
  prettier.
- **Access denied.** `OpenProcess` fails for protected/elevated/system
  processes → fall back to `PID n`, and never let that win the ranking.
- **UWP/MSIX ms-resource names.** Need WinRT `PackageManager` for a clean
  display name; the plain-Win32 path only yields the package identity.
- **Device changes mid-recording.** Sottovoce re-follows the default device on
  switch. The sampler must re-resolve the endpoint on the same 500 ms watch as the
  capture, or accumulate per-endpoint and merge at the end.
- **Privacy/expectation.** The name is derived from whatever was loudest; a music
  player or a notification could win. Keep it as a *suggestion* with a fallback.

## Hypotheses to confirm (not verified here)

- Browser/Electron sessions resolve to the main process vs a child process, and
  grouping by name reliably merges them. Needs a real Slack/Teams/Chrome call.
- `GetPeakValue` behaves correctly for sessions whose process is elevated or
  packaged — observed OK for packaged (ChatGPT), not tested for elevated.
- `ms-resource:` manifests for WhatsApp/Store Teams resolve via
  `PackageManager`; the prototype falls back instead.
