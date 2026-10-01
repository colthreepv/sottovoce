# FFmpeg sourcing for Sottovoce: minimal build vs. prebuilt vs. in-process

Date: 2026-10-02. Research note (Seeker), read-only investigation.
Scope: this document only. No code changes were made.

Labels used below:
- **Verified (repo)** — read directly from this checkout, with file:line.
- **Verified (web)** — from the live pages linked, current as of the search date.
- **Hypothesis** — my reading or industry knowledge, not yet confirmed. Confirm before acting.

## 1. What the code actually asks FFmpeg to do

Every FFmpeg call in the tree, grouped by purpose. All go through
src/ffmpeg.rs, which looks for ffmpeg.exe next to the app, then on PATH.

**Verified (repo).**

| Purpose | Command shape | Site |
|---|---|---|
| Locate binary | ffmpeg.exe beside exe, else PATH | src/ffmpeg.rs:10 |
| Base flags | -hide_banner -nostdin -loglevel error + CREATE_NO_WINDOW | src/ffmpeg.rs:27 |
| Health check | -encoders must contain libopus | src/ffmpeg.rs:39 |
| Decode to f32 | -i IN -vn -f f32le -acodec pcm_f32le -ar R -ac C pipe:1 | src/ffmpeg.rs:60 |
| Encode as it records | -y -f s16le -ar R -ac 1 -i pipe:0 -map_metadata -1 -c:a libopus -b:a 48000 -vbr on -application voip -f ogg OUT | src/capture.rs:742 |
| PCM feed + silence padding | samples written to the child's stdin; gaps padded with silence | src/capture.rs:680 |
| Decode for playback | decode(path, 48000, 1) for mic.ogg + computer.ogg, mixed in Rust | src/player.rs:164 |
| Decode for diarization/peak | decode_mono_16k(path) | src/pipeline.rs:68 |
| Recover duration after crash | -i IN -progress pipe:1 -nostats -f null - | src/meetings.rs:281 |
| CLI/selftest decodes | decode_mono_16k | src/main.rs:185, src/main.rs:324, src/main.rs:441 |
| Test-only tone (lavfi) | -f lavfi -i sine=... -c:a libopus (gui-record-test) | src/main.rs:149 |

So FFmpeg is used for exactly four production jobs: **Opus encode on a live
stdin PCM stream**, **f32 decode**, **16 kHz mono decode**, and a **null-sink
duration probe**. Plus one test-only lavfi tone generator.

Required components (from the Dockerfile enable list): encoder libopus, muxer
ogg, decoder opus plus the import set (vorbis, mp3, aac, flac, alac, pcm_*),
demuxers ogg, wav, mp3, mov, matroska, flac, aac, pcm_s16le, protocols
file+pipe, filters aresample, aformat, anull, abuffer, abuffersink.

## 2. Current build, and the one real gap

**Verified (repo).** packaging/Dockerfile cross-compiles on
debian:bookworm-slim with mingw-w64 against FFmpeg **8.1.3** and libopus
**1.5.2**, LGPL only (--disable-gpl --disable-version3 --disable-nonfree),
static, stripped, with a hard < 10 MB guard. Sources are SHA-256 pinned twice:
downloads in packaging/build-ffmpeg.ps1 ($archivePins) and re-checked inside
the Dockerfile with sha256sum -c. packaging/verify-ffmpeg.ps1 then proves
libopus presence, an s16le to Ogg/Opus round trip, bit-exact sample counts on
the .ogg fixtures, and mp3/m4a/wav import. package.ps1 emits the < 10 MB check
and bundles the LGPL/BSD license texts. (Docker present here: 29.8.0, buildx
0.37.1.)

**Gap (verified by configure flags; not runtime-tested — no built binary is
present).** The bundled binary enables --enable-muxer=ogg,pcm_s16le,pcm_f32le,wav
but **not the null muxer** and not lavfi. The crash-recovery probe in
src/meetings.rs uses -f null -, so with the bundled binary that path fails and
duration silently falls back to 0 (the error is appended to session.errors;
recovery still completes). main.rs's gui-record-test tone also needs lavfi, so
it only works against a system FFmpeg. Recommend --enable-muxer=null now;
lavfi is test-only and can stay on the system binary.

## 3. Comparison

### (a) Keep our own minimal Docker build

- Control: smallest artifact by far (~4 MB guard), LGPL-only, exactly the
  components we use. Prebuilt vendors do not offer anything this lean.
- Verifiability today: two SHA-256 gates, but **no signature check**. FFmpeg
  publishes GPG-signed tarballs (*.tar.xz.asc, keys listed on ffmpeg.org) and
  Xiph signs the libopus tarball. Adding gpg --verify turns "hash matches what
  we pinned" into "hash matches an upstream signature."
- Update tracking: manual today — pins live in build-ffmpeg.ps1 and the notices
  file, and nothing watches for new releases. Options: a weekly scheduled CI
  job that checks ffmpeg.org/releases/ for a newer 8.1.x/current tarball, plus a
  CVE feed (FFmpeg security page and the ffmpeg-security list). Dependabot will
  not see tar.xz pins.
- Cost: Docker required. Each bump is a human edit in three places (Dockerfile
  ARG, build-ffmpeg.ps1, notices + hashes).

### (b) Prebuilt Windows builds

**Verified (web).** FFmpeg's download page points Windows users at gyan.dev and
BtbN. Facts as of the search date:

- **BtbN/FFmpeg-Builds** — daily autobuilds from git master plus pinned
  releases; publishes **both GPL and LGPL** variants, static and shared,
  win64/win32. Releases carry per-asset **SHA-256** (checksum files alongside
  assets); latest release observed Sep 27, 2026.
  https://github.com/BtbN/FFmpeg-Builds/releases and
  https://github.com/BtbN/FFmpeg-Builds/wiki/Latest
- **gyan.dev** — latest observed 9.0.2 (Sep 19, 2026); "Essentials" ~34 MB. The
  default/essentials/full builds are **GPLv3, not LGPL**; a "full-shared"
  variant exists for linking. Publishes .sha256 files.
  https://www.gyan.dev/ffmpeg/builds/
- **FFmpeg project status (web):** newest series is **9.0.2** (Sep 18, 2026);
  the **8.1** branch has **8.1.3** (Sep 21, 2026) and **8.0.3** (Jun 18, 2026)
  — 8.0/8.1 still receive point (security) releases. The security page lists
  recent CVEs fixed across those branches.
  https://ffmpeg.org/security.html and https://ffmpeg.org/download.html

Notes:
- Licensing: our product stays LGPL-compatible only if we ship an LGPL build.
  gyan.dev's normal builds are GPL — using one would force the whole
  distributed bundle into GPL compliance. BtbN's ...-lgpl assets are the only
  prebuilt option that fits without changing our license posture.
- Size: prebuilts are full-feature, so much larger than our 4 MB (tens of MB —
  **hypothesis**, confirm exact current asset size at pin time). "Minimal" is
  not a vendor offering.
- Verifiability: GitHub Actions builds with published SHA-256 are decent; gyan
  also publishes hashes. Neither matches our own reproducible build if
  provenance is a goal.

### (c) Drop the external process: in-process Rust encode/decode

Two halves, and they can land separately.

**Encoding (Opus + Ogg mux).** Replace the libopus child process with a crate
binding libopus plus a pure-Rust Ogg writer. **Hypothesis** on names: the opus
crate (bindings; older sibling audiopus) and the ogg crate's PacketWriter cover
this; libopus is static-linked and BSD-licensed, matching what we already ship.
The existing spawn_writer/write_silence packet-and-padding logic
(src/capture.rs:680) stays; only the sink changes from a child's stdin to an
in-process encoder + Ogg writer. Highest-value change: it removes process spawn,
stdin pipe, CREATE_NO_WINDOW, child-death, and PATH/bundling issues from the
recording hot path.

**Decoding.** **Hypothesis:** symphonia (pure Rust, MPL-2.0) decodes Opus in
Ogg (plus wav/mp3/aac-in-mp4/flac/vorbis via its format and codec crates), and
rubato (pure Rust, adjacent to our existing realfft dep) does the resample to
16 kHz. That covers playback (mic.ogg+computer.ogg to 48 kHz stereo mix) and
diarization (to 16 kHz mono) in one process. Weak spots to check: matroska/odd
imports and exact Opus-in-Ogg granule handling — keep the FFmpeg path behind a
feature flag until fixture parity passes.

Risks / trade-offs:
- Opus frame/granule bookkeeping is the fiddly part; FFmpeg hides it today.
- Loss of "any format" import; use verify-ffmpeg.ps1's fixture set as the bar.
- New -sys build (cc/cmake) on Windows MSVC; pin libopus the same way we do now.
- Upside: single binary, no 4 MB sidecar, and no LGPL "offer to relink"
  question for a statically linked shipped executable. The crash-recovery probe
  becomes an in-process count instead of a -f null subprocess. Net binary effect
  is likely smaller than app+FFmpeg (**hypothesis**; measure — static libopus is
  a few hundred KB stripped vs ~4 MB for ffmpeg.exe).

## 4. Recommendation

**Target state: (c) in-process**, implemented after near-term hardening of (a).
Keep (b) as a contingency only.

Our FFmpeg surface is tiny and fixed (one encoder, one decoder, one muxer, one
resampler), which is exactly the case where an external multi-format binary
earns nothing and costs process management, bundling, PATH discovery, a 4 MB
sidecar, and LGPL static-linkage obligations on a shipped executable. In-process
removes the "where is ffmpeg.exe" failure class entirely and fits the Tauri 2
phase (the Rust engine stays). Until it lands, the pinned Docker build is the
right source: smallest artifact, LGPL-clean, reproducible. BtbN LGPL static is
the fallback if Docker becomes a blocker, accepting a much larger artifact.

## 5. Migration plan

1. **Harden (a) now — small, low-risk.**
   - Add --enable-muxer=null so crash-recovery duration works with the bundled
     binary.
   - Add GPG verification of ffmpeg-*.tar.xz and opus-*.tar.gz against upstream
     keys in build-ffmpeg.ps1 before the SHA-256 check.
   - Add a weekly scheduled CI job watching ffmpeg.org/releases/ (and the
     security page) that opens an issue on a new release.
2. **Bump the branch deliberately.** 8.1.3 is already the newest 8.1.x; decide
   separately whether to track 9.0. Either way update ARG + both hash sites +
   THIRD-PARTY-NOTICES.txt together.
3. **In-process encode first.** Introduce Opus+Ogg encode behind a feature flag;
   keep spawn_encoder as default until Ogg output is sane (duration, bitrate,
   voip mode) and the padding logic is preserved. This removes the
   recording-path process.
4. **In-process decode second.** Add symphonia+rubato for playback and
   diarization decodes; gate on the existing fixture comparison (sample counts,
   16 kHz mono f32).
5. **Retire ffmpeg.exe.** Once fixtures pass with FFmpeg absent, drop the
   sidecar from package.ps1, remove the < 10 MB FFmpeg guard and the
   LGPL/BSD FFmpeg notices, and update the README (its current "must be on
   PATH" line is already stale versus the bundled build).

## 6. Open questions to confirm before acting

- Exact current sizes and SHA-256 of the BtbN LGPL static win64 asset and the
  gyan Essentials asset (needed only if we fall back to b).
- Current crate versions/licences for opus, ogg, symphonia, rubato, and whether
  symphonia's Opus decoder is feature-complete for our ogg files.
- Whether to keep an optional FFmpeg escape hatch for exotic imports after step 5.

