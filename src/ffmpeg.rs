//! FFmpeg: found next to the executable first (the bundled one), then on PATH.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Keeps a console window from flashing up for every FFmpeg run in the GUI.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for candidate in [
            dir.join("ffmpeg.exe"),
            dir.join("ffmpeg").join("ffmpeg.exe"),
        ] {
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("ffmpeg")
}

/// An FFmpeg command without a console window and without banner noise.
pub fn command() -> Command {
    let mut command = Command::new(path());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
    command
}

/// Fails with a readable message unless FFmpeg runs and has libopus.
pub fn check() -> Result<(), String> {
    let output = command()
        .args(["-encoders"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            format!("FFmpeg not found (put ffmpeg.exe next to the app or on PATH): {e}")
        })?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !text.contains("libopus") {
        return Err("FFmpeg was found but it has no libopus encoder".to_owned());
    }
    Ok(())
}

/// Decodes any audio file to 16 kHz mono f32, the rate Nemotron wants.
pub fn decode_mono_16k(input: &Path) -> Result<Vec<f32>, String> {
    decode(input, 16_000, 1)
}

/// Decodes any audio file to interleaved f32 at `rate` with `channels`.
pub fn decode(input: &Path, rate: u32, channels: u16) -> Result<Vec<f32>, String> {
    let output = command()
        .arg("-i")
        .arg(input)
        .args(["-vn", "-f", "f32le", "-acodec", "pcm_f32le"])
        .args(["-ar", &rate.to_string(), "-ac", &channels.to_string()])
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not run FFmpeg: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "FFmpeg could not decode {}: {}",
            input.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}
