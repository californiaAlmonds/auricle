//! Optional third-party add-on management (yt-dlp, ffmpeg).
//!
//! Auricle does NOT bundle or redistribute these tools. They are downloaded only
//! when the user explicitly chooses to install them (the in-app "Install essential
//! add-ons" step), into a per-user directory. All resolution also honours tools that
//! are already present on the system `PATH`, so users may install them manually.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

const YTDLP_WIN_URL: &str =
    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
const FFMPEG_WIN_ZIP_URL: &str =
    "https://github.com/BtbN/FFmpeg-Builds/releases/latest/download/ffmpeg-master-latest-win64-gpl.zip";

/// Per-user directory where downloaded add-on binaries are stored:
/// `%LOCALAPPDATA%\auricle\bin\` on Windows.
pub fn addon_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join("auricle").join("bin")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".auricle").join("bin")
    }
}

fn exe_name(tool: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    }
}

/// Resolve a tool's path: per-user add-on dir → next to the running exe → bare name (PATH).
pub fn resolve_tool(tool: &str) -> PathBuf {
    let file = exe_name(tool);

    let in_dir = addon_dir().join(&file);
    if in_dir.exists() {
        return in_dir;
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for sub in ["", "bin", "resources"] {
                let p = if sub.is_empty() {
                    dir.join(&file)
                } else {
                    dir.join(sub).join(&file)
                };
                if p.exists() {
                    return p;
                }
            }
        }
    }

    // Fall back to bare name so the OS resolves it from PATH.
    PathBuf::from(file)
}

fn detect(tool: &str, version_arg: &str) -> bool {
    let mut cmd = Command::new(resolve_tool(tool));
    cmd.arg(version_arg);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    matches!(cmd.output(), Ok(o) if o.status.success())
}

/// True if a usable `yt-dlp` is available (add-on dir or PATH).
pub fn ytdlp_installed() -> bool {
    detect("yt-dlp", "--version")
}

/// True if a usable `ffmpeg` is available (add-on dir or PATH).
pub fn ffmpeg_installed() -> bool {
    detect("ffmpeg", "-version")
}

/// Download `url` into memory, reporting progress through `on_progress`.
///
/// `on_progress` receives a fraction in `0.0..=1.0` when the server reports a
/// `Content-Length`, or `-1.0` (once) when the total size is unknown, signalling
/// that the UI should fall back to an indeterminate bar.
fn http_download(url: &str, on_progress: &dyn Fn(f32)) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .user_agent("Auricle")
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }

    let total = resp.content_length();
    if total.is_none() {
        on_progress(-1.0);
    }

    let cap = total.unwrap_or(0) as usize;
    let mut out: Vec<u8> = Vec::with_capacity(cap);
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    loop {
        let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        downloaded += n as u64;
        if let Some(total) = total {
            if total > 0 {
                on_progress((downloaded as f32 / total as f32).min(1.0));
            }
        }
    }
    Ok(out)
}

/// Download `yt-dlp` into the per-user add-on directory. Windows only for now.
///
/// `on_progress` reports the download fraction (`0.0..=1.0`, or `-1.0` for an
/// unknown total).
pub fn install_ytdlp(on_progress: impl Fn(f32) + Send) -> Result<(), String> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = on_progress;
        Err("Automatic install is only supported on Windows. Please install yt-dlp manually.".into())
    }
    #[cfg(target_os = "windows")]
    {
        let dir = addon_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let bytes = http_download(YTDLP_WIN_URL, &on_progress)?;
        let dest = dir.join("yt-dlp.exe");
        let tmp = dir.join("yt-dlp.exe.part");
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
            f.write_all(&bytes).map_err(|e| e.to_string())?;
            f.flush().ok();
        }
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        on_progress(1.0);
        Ok(())
    }
}

/// Download `ffmpeg` (and `ffprobe`) into the per-user add-on directory. Windows only.
///
/// `on_progress` maps the download to `0.0..=0.9` and the archive extraction to
/// `0.9..=1.0` (or reports `-1.0` when the download size is unknown).
pub fn install_ffmpeg(on_progress: impl Fn(f32) + Send) -> Result<(), String> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = on_progress;
        Err("Automatic install is only supported on Windows. Please install ffmpeg manually.".into())
    }
    #[cfg(target_os = "windows")]
    {
        let dir = addon_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let bytes = http_download(FFMPEG_WIN_ZIP_URL, &|p: f32| {
            on_progress(if p < 0.0 { p } else { p * 0.9 });
        })?;
        let reader = std::io::Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(reader).map_err(|e| e.to_string())?;

        let count = archive.len().max(1);
        let mut extracted = 0;
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).map_err(|e| e.to_string())?;
            let name = file.name().to_lowercase();
            let target = if name.ends_with("/ffmpeg.exe") || name == "ffmpeg.exe" {
                Some("ffmpeg.exe")
            } else if name.ends_with("/ffprobe.exe") || name == "ffprobe.exe" {
                Some("ffprobe.exe")
            } else {
                None
            };
            if let Some(tname) = target {
                let dest = dir.join(tname);
                let mut out = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
                std::io::copy(&mut file, &mut out).map_err(|e| e.to_string())?;
                extracted += 1;
            }
            on_progress(0.9 + 0.1 * ((i + 1) as f32 / count as f32));
        }

        if extracted == 0 {
            return Err("ffmpeg.exe not found in downloaded archive".into());
        }
        on_progress(1.0);
        Ok(())
    }
}
