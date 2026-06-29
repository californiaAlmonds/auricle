//! In-app update support for the **portable** build only.
//!
//! Installer/Store builds (scoop/winget/chocolatey/msix) are compiled with
//! `--no-default-features`, so the entire self-replacing flow in [`apply_update`]
//! is excluded and the relevant UI is hidden. Those channels are updated by their
//! package manager or the Microsoft Store.
//!
//! The lightweight version *check* is always compiled so a build could, in theory,
//! still surface "up to date" info, but the portable build is the only one that
//! exposes it (see `self_update_enabled` in the UI wiring).

#[cfg(feature = "self-update")]
const OWNER: &str = "californiaAlmonds";
#[cfg(feature = "self-update")]
const REPO: &str = "auricle";

/// Current version baked in at compile time, e.g. `0.1.1`.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// True when the binary includes the self-updating capability (portable build).
pub fn self_update_supported() -> bool {
    cfg!(feature = "self-update")
}

/// Result of comparing the running version against the latest GitHub release.
pub struct UpdateCheck {
    /// Latest tag's semver, stripped of any leading `v`.
    pub latest: String,
    /// True when a newer stable release is available.
    pub update_available: bool,
}

/// Query the latest GitHub release and compare it to the running version.
///
/// Blocking; call from a background thread. Errors map to a user-facing string.
#[cfg(feature = "self-update")]
pub fn check_latest() -> Result<UpdateCheck, String> {
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .build()
        .map_err(|e| e.to_string())?
        .fetch()
        .map_err(|e| e.to_string())?;

    let latest = releases
        .into_iter()
        .map(|r| r.version)
        .next()
        .ok_or_else(|| "no releases found".to_string())?;

    let current = current_version();
    let update_available =
        self_update::version::bump_is_greater(current, &latest).unwrap_or(false);

    Ok(UpdateCheck { latest, update_available })
}

#[cfg(not(feature = "self-update"))]
pub fn check_latest() -> Result<UpdateCheck, String> {
    Err("update checks are disabled in this build".into())
}

/// Download and self-replace the running executable with the latest release.
///
/// Portable build only. On success the new binary is in place; the caller should
/// prompt the user to restart. Blocking; call from a background thread.
#[cfg(feature = "self-update")]
pub fn apply_update() -> Result<String, String> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .bin_name("auricle")
        .show_download_progress(false)
        .current_version(current_version())
        .build()
        .map_err(|e| e.to_string())?
        .update()
        .map_err(|e| e.to_string())?;
    Ok(status.version().to_string())
}

#[cfg(not(feature = "self-update"))]
pub fn apply_update() -> Result<String, String> {
    Err("self-update is not available in this build".into())
}
