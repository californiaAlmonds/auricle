//! Local diagnostics: a size-capped log file, panic capture, startup error
//! dialogs and a user-initiated "Report a problem" flow. Nothing is ever sent
//! automatically; reports open a prefilled GitHub issue the user reviews first.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 1024 * 1024;
const ISSUES_NEW_URL: &str = "https://github.com/californiaAlmonds/auricle/issues/new";

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();
static LOGGER: FileLogger = FileLogger;
static STDERR_REDIRECTED: AtomicBool = AtomicBool::new(false);
static WRITES_SINCE_SIZE_CHECK: AtomicU32 = AtomicU32::new(0);

pub fn log_dir() -> PathBuf {
    crate::core::persistence::data_dir().join("logs")
}

pub fn log_path() -> PathBuf {
    log_dir().join("auricle.log")
}

fn previous_log_path() -> PathBuf {
    log_dir().join("auricle.1.log")
}

fn version() -> &'static str {
    crate::core::updater::current_version()
}

fn build_flavor() -> &'static str {
    if crate::core::updater::self_update_supported() { "portable" } else { "installer" }
}

/// Opens the log file, routes `log` macros and (for GUI builds without a console)
/// stderr into it, installs the panic hook and records the session header.
pub fn init() {
    if LOG_FILE.get().is_some() {
        return;
    }
    // Settings are cached on first read, so legacy data must be folded in first.
    crate::core::persistence::migrate_legacy_dir();
    let _ = std::fs::create_dir_all(log_dir());
    let path = log_path();
    if std::fs::metadata(&path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(&path, previous_log_path());
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    #[cfg(windows)]
    redirect_stderr_if_detached(&file);
    let _ = LOG_FILE.set(Mutex::new(file));
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    install_panic_hook();

    write_line("==================== session start ====================");
    write_line(&format!("Auricle {} ({} build)", version(), build_flavor()));
    write_line(&format!("Windows: {}", os_description()));
    if let Ok(exe) = std::env::current_exe() {
        write_line(&format!("Executable: {}", exe.display()));
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    write_line(&format!("Arguments: {args:?}"));
    write_line(&format!("Data folder: {}", crate::core::persistence::data_dir().display()));
    if let Ok(backend) = std::env::var("SLINT_BACKEND") {
        write_line(&format!("SLINT_BACKEND={backend}"));
    }
    write_line(&format!(
        "Browser cookies for YouTube: {}",
        if crate::core::persistence::load_settings().use_browser_cookies { "enabled by user" } else { "disabled" }
    ));
}

/// Appends one timestamped line to the log file (no-op before [`init`]).
pub fn write_line(line: &str) {
    if let Some(file) = LOG_FILE.get() {
        if let Ok(mut f) = file.lock() {
            let _ = writeln!(f, "{} {line}", timestamp());
            if WRITES_SINCE_SIZE_CHECK.fetch_add(1, Ordering::Relaxed) >= 200 {
                WRITES_SINCE_SIZE_CHECK.store(0, Ordering::Relaxed);
                rotate_if_full(&mut f);
            }
        }
    }
}

/// Keeps the log bounded during long sessions: the full file becomes
/// `auricle.1.log` (replacing the older one) and a fresh file is started.
fn rotate_if_full(current: &mut File) {
    if current.metadata().map(|m| m.len() <= MAX_LOG_BYTES).unwrap_or(true) {
        return;
    }
    if std::fs::rename(log_path(), previous_log_path()).is_err() {
        return;
    }
    if let Ok(fresh) = OpenOptions::new().create(true).append(true).open(log_path()) {
        #[cfg(windows)]
        if STDERR_REDIRECTED.load(Ordering::Relaxed) {
            point_stderr_at(&fresh);
        }
        *current = fresh;
    }
}

struct FileLogger;

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        if metadata.target().starts_with("auricle") {
            metadata.level() <= log::Level::Info
        } else {
            metadata.level() <= log::Level::Warn
        }
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            write_line(&format!("{:<5} {}: {}", record.level(), record.target(), record.args()));
        }
    }

    fn flush(&self) {}
}

/// Release builds use the GUI subsystem, so `eprintln!` output normally goes
/// nowhere; point stderr at the log file instead. Kept as-is when a console exists.
#[cfg(windows)]
fn redirect_stderr_if_detached(file: &File) {
    use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};

    let has_stderr = matches!(unsafe { GetStdHandle(STD_ERROR_HANDLE) }, Ok(h) if !h.is_invalid());
    if has_stderr {
        return;
    }
    point_stderr_at(file);
    STDERR_REDIRECTED.store(true, Ordering::Relaxed);
}

#[cfg(windows)]
fn point_stderr_at(file: &File) {
    use std::os::windows::io::IntoRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE};

    // The duplicated handle is intentionally never closed: stderr may use it at any time.
    if let Ok(clone) = file.try_clone() {
        let raw = clone.into_raw_handle();
        unsafe {
            let _ = SetStdHandle(STD_ERROR_HANDLE, HANDLE(raw));
        }
    }
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>").to_string();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        let location = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        write_line(&format!("[panic] thread={name} at {location}: {message}"));
        write_line(&format!("[panic] backtrace:\n{}", std::backtrace::Backtrace::force_capture()));
        if name == "main" {
            show_error_dialog(&format!("Auricle hit an unexpected error and has to close: {message}"));
        }
        default_hook(info);
    }));
}

/// Logs `message` and shows a blocking error box offering to report it on GitHub.
pub fn show_error_dialog(message: &str) {
    write_line(&format!("[error-dialog] {message}"));
    #[cfg(windows)]
    {
        use windows::core::{HSTRING, PCWSTR};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, IDYES, MB_ICONERROR, MB_SETFOREGROUND, MB_YESNO,
        };
        let text = HSTRING::from(format!(
            "{message}\n\nDetails were saved to:\n{}\n\nReport this problem on GitHub?\n\nYes opens a GitHub issue form in your browser with the app version, Windows version and this error (your user folder is masked), and opens the log folder so you can choose whether to attach auricle.log. The issue is only created if you submit it, and GitHub issues are public.",
            redact(&log_path().display().to_string())
        ));
        let caption = HSTRING::from("Auricle");
        let choice = unsafe {
            MessageBoxW(
                HWND::default(),
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_YESNO | MB_ICONERROR | MB_SETFOREGROUND,
            )
        };
        if choice == IDYES {
            open_report(message);
        }
    }
}

/// Opens a prefilled GitHub issue (version, OS, masked error; no log lines) and
/// reveals the log so the user can choose to attach it. User-initiated only.
pub fn open_report(context: &str) {
    write_line("[report] opening GitHub issue form and log folder");
    open_in_shell(&report_url(context));
    reveal_log_file();
}

pub fn open_log_folder() {
    let _ = std::fs::create_dir_all(log_dir());
    reveal_log_file();
}

fn reveal_log_file() {
    let path = log_path();
    if path.exists() {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    } else {
        open_in_shell(&log_dir().to_string_lossy());
    }
}

fn open_in_shell(target: &str) {
    #[cfg(windows)]
    {
        use windows::core::{w, HSTRING, PCWSTR};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let target = HSTRING::from(target);
        unsafe {
            ShellExecuteW(HWND::default(), w!("open"), PCWSTR(target.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
        }
    }
    #[cfg(not(windows))]
    let _ = target;
}

fn report_url(context: &str) -> String {
    let context = redact(context.trim());
    let title = if context.is_empty() {
        "Problem report".to_string()
    } else {
        let first = context.lines().next().unwrap_or_default();
        format!("Problem report: {}", first.chars().take(80).collect::<String>())
    };
    let mut body = format!(
        "**What happened?**\n<!-- Describe what you were doing and what went wrong. -->\n\n**Auricle:** {} ({} build)\n**Windows:** {}\n",
        version(),
        build_flavor(),
        os_description()
    );
    if !context.is_empty() {
        let short: String = context.chars().take(600).collect();
        body.push_str(&format!("**Error:** {short}\n"));
    }
    body.push_str("\n**Log:** if you're comfortable sharing it, drag `auricle.log` from the folder that opened into this issue. It can include song titles, video IDs and search terms, and GitHub issues are public.\n");
    format!("{ISSUES_NEW_URL}?title={}&body={}", encode(&title), encode(&body))
}

/// Masks the user's profile path and account name (case-insensitive, either slash style).
fn redact(text: &str) -> String {
    let mut out = text.to_string();
    if let Ok(profile) = std::env::var("USERPROFILE") {
        if !profile.is_empty() {
            out = replace_ignore_ascii_case(&out, &profile, "%USERPROFILE%");
            out = replace_ignore_ascii_case(&out, &profile.replace('\\', "/"), "%USERPROFILE%");
        }
    }
    if let Ok(user) = std::env::var("USERNAME") {
        if user.len() >= 3 {
            out = replace_ignore_ascii_case(&out, &format!("\\{user}\\"), "\\<user>\\");
            out = replace_ignore_ascii_case(&out, &format!("/{user}/"), "/<user>/");
        }
    }
    out
}

fn replace_ignore_ascii_case(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    // ASCII lowercasing keeps byte offsets aligned with the original string.
    let lower_hay = haystack.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut last = 0;
    for (start, _) in lower_hay.match_indices(&lower_needle) {
        if start < last {
            continue;
        }
        out.push_str(&haystack[last..start]);
        out.push_str(replacement);
        last = start + needle.len();
    }
    out.push_str(&haystack[last..]);
    out
}

fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Handles `--version` and `--help` for the GUI-subsystem executable.
/// Returns true when the process should exit without opening the window.
pub fn handle_cli_args() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |names: &[&str]| args.iter().any(|a| names.iter().any(|n| a.eq_ignore_ascii_case(n)));
    let text = if has(&["--version", "-v", "-V", "/version"]) {
        format!("Auricle {}", version())
    } else if has(&["--help", "-h", "/?", "/h", "/help"]) {
        format!(
            "Auricle {} - native Windows music player\n\nUsage: auricle.exe [--version | --help]\n\nRun without arguments to open the app.\nLog file: {}\nPrivacy:  https://californiaalmonds.github.io/auricle/privacy.html\nIssues:   https://github.com/californiaAlmonds/auricle/issues",
            version(),
            log_path().display()
        )
    } else {
        return false;
    };
    print_to_parent_console(&text);
    true
}

fn print_to_parent_console(text: &str) {
    #[cfg(windows)]
    {
        use windows::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
        // Keep an inherited pipe/file (e.g. `> out.txt`); only fall back to the parent console.
        let redirected = matches!(unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }, Ok(h) if !h.is_invalid());
        if !redirected {
            let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
        }
    }
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

/// True once the main window is visible on screen.
pub fn window_visible() -> bool {
    native_window_state().as_deref().is_some_and(|s| s.contains("visible=true"))
}

/// Records where the main window ended up.
pub fn log_window_state(window: &slint::Window) {
    let pos = window.position();
    let size = window.size();
    write_line(&format!(
        "[startup] window state: slint pos=({},{}) size={}x{} scale={:.2}; win32 {}",
        pos.x,
        pos.y,
        size.width,
        size.height,
        window.scale_factor(),
        native_window_state().as_deref().unwrap_or("window not found")
    ));
}

/// Called when the window never became visible after startup: logs and offers a report.
pub fn window_never_shown(window: &slint::Window) {
    log_window_state(window);
    log::warn!("[startup] main window never became visible");
    std::thread::spawn(|| {
        show_error_dialog("Auricle started, but its main window did not appear.");
    });
}

#[cfg(windows)]
fn native_window_state() -> Option<String> {
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowExW, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    };
    unsafe {
        let our_pid = std::process::id();
        let mut child = HWND::default();
        loop {
            let h = FindWindowExW(HWND::default(), child, None, w!("Auricle")).ok()?;
            if h.0.is_null() {
                return None;
            }
            child = h;
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            if pid == our_pid {
                let mut rect = RECT::default();
                let _ = GetWindowRect(h, &mut rect);
                return Some(format!(
                    "visible={} minimized={} rect=({},{})-({},{})",
                    IsWindowVisible(h).as_bool(),
                    IsIconic(h).as_bool(),
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.bottom
                ));
            }
        }
    }
}

#[cfg(not(windows))]
fn native_window_state() -> Option<String> {
    Some("visible=true (not checked on this platform)".to_string())
}

fn os_description() -> String {
    #[cfg(windows)]
    {
        let product = reg_string("ProductName").unwrap_or_else(|| "Windows".to_string());
        let display = reg_string("DisplayVersion").unwrap_or_default();
        let build = reg_string("CurrentBuildNumber").unwrap_or_default();
        let ubr = reg_dword("UBR").map(|u| format!(".{u}")).unwrap_or_default();
        format!("{product} {display} (build {build}{ubr})").replace("  ", " ")
    }
    #[cfg(not(windows))]
    {
        std::env::consts::OS.to_string()
    }
}

#[cfg(windows)]
fn reg_string(value: &str) -> Option<String> {
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let name = HSTRING::from(value);
    let mut buf = [0u16; 256];
    let mut size = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = (size as usize / 2).min(buf.len());
    let text = String::from_utf16_lossy(&buf[..len]);
    Some(text.trim_end_matches('\0').to_string())
}

#[cfg(windows)]
fn reg_dword(value: &str) -> Option<u32> {
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
    let name = HSTRING::from(value);
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut data as *mut u32).cast()),
            Some(&mut size),
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}

fn timestamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        now.subsec_millis()
    )
}

/// Days since 1970-01-01 to a (year, month, day) civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_392), (2025, 10, 31));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn encode_reserves_url_syntax() {
        assert_eq!(encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }

    #[test]
    fn report_url_has_no_log_lines_and_masks_profile() {
        let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\someone".into());
        let url = report_url(&format!("Something failed in {}\\x\nsecond line", profile.to_ascii_uppercase()));
        assert!(url.starts_with(ISSUES_NEW_URL));
        assert!(url.contains("title=Problem%20report%3A%20Something%20failed%20in%20%25USERPROFILE%25"), "{url}");
        assert!(!url.contains("session%20start"));
    }

    #[test]
    fn replace_ignores_ascii_case() {
        assert_eq!(replace_ignore_ascii_case("A c:\\USERS\\Bob z", "C:\\Users\\bob", "X"), "A X z");
    }
}
