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

fn http_get_bytes(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .user_agent("Auricle")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    resp.bytes().map(|b| b.to_vec()).map_err(|e| e.to_string())
}

/// Download `yt-dlp` into the per-user add-on directory. Windows only for now.
pub fn install_ytdlp() -> Result<(), String> {
    #[cfg(not(target_os = "windows"))]
    {
        Err("Automatic install is only supported on Windows. Please install yt-dlp manually.".into())
    }
    #[cfg(target_os = "windows")]
    {
        let dir = addon_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let bytes = http_get_bytes(YTDLP_WIN_URL)?;
        let dest = dir.join("yt-dlp.exe");
        let tmp = dir.join("yt-dlp.exe.part");
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
            f.write_all(&bytes).map_err(|e| e.to_string())?;
            f.flush().ok();
        }
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Download `ffmpeg` (and `ffprobe`) into the per-user add-on directory. Windows only.
pub fn install_ffmpeg() -> Result<(), String> {
    #[cfg(not(target_os = "windows"))]
    {
        Err("Automatic install is only supported on Windows. Please install ffmpeg manually.".into())
    }
    #[cfg(target_os = "windows")]
    {
        let dir = addon_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let bytes = http_get_bytes(FFMPEG_WIN_ZIP_URL)?;
        let reader = std::io::Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(reader).map_err(|e| e.to_string())?;

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
        }

        if extracted == 0 {
            return Err("ffmpeg.exe not found in downloaded archive".into());
        }
        Ok(())
    }
}
