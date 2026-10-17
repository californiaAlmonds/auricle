use std::cell::RefCell;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};

#[derive(Clone, Copy, Debug)]
pub enum MediaAction {
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    SeekTo(f64),
    SeekBy(f64),
}

#[derive(Clone, Debug)]
pub struct MediaSnapshot {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_secs: u32,
    pub elapsed_secs: f64,
    pub is_playing: bool,
}

#[derive(PartialEq, Eq)]
struct PushedMeta {
    video_id: String,
    title: String,
    artist: String,
    album: String,
    duration_secs: u32,
    has_local_thumb: bool,
}

struct PushedPlayback {
    video_id: String,
    is_playing: bool,
    base_elapsed_secs: f64,
    pushed_at: Instant,
}

struct State {
    controls: MediaControls,
    last_meta: Option<PushedMeta>,
    last_playback: Option<PushedPlayback>,
    is_stopped: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn log_error(msg: &str) {
    let full = format!("[media-controls] {msg}");
    log::error!("{full}");
    eprintln!("{full}");
}

/// Initialize Windows System Media Transport Controls (SMTC) on the UI thread.
/// Idempotent: subsequent calls after a successful attach are a no-op.
pub fn attach(hwnd: isize, on_action: impl Fn(MediaAction) + Send + 'static) {
    if hwnd == 0 {
        log_error("attach called with null HWND; skipping");
        return;
    }

    let already_attached = STATE.with(|cell| cell.borrow().is_some());
    if already_attached {
        return;
    }

    let config = PlatformConfig {
        display_name: "Auricle",
        dbus_name: "auricle",
        hwnd: Some(hwnd as *mut c_void),
    };

    let mut controls = match MediaControls::new(config) {
        Ok(c) => c,
        Err(e) => {
            log_error(&format!("failed to create MediaControls: {e:?}"));
            return;
        }
    };

    let attach_res = controls.attach(move |event| {
        let action = match event {
            MediaControlEvent::Toggle => Some(MediaAction::PlayPause),
            MediaControlEvent::Play => Some(MediaAction::Play),
            MediaControlEvent::Pause | MediaControlEvent::Stop => Some(MediaAction::Pause),
            MediaControlEvent::Next => Some(MediaAction::Next),
            MediaControlEvent::Previous => Some(MediaAction::Previous),
            MediaControlEvent::SetPosition(MediaPosition(pos)) => {
                Some(MediaAction::SeekTo(pos.as_secs_f64()))
            }
            MediaControlEvent::Seek(SeekDirection::Forward) => Some(MediaAction::SeekBy(10.0)),
            MediaControlEvent::Seek(SeekDirection::Backward) => Some(MediaAction::SeekBy(-10.0)),
            _ => None,
        };
        if let Some(act) = action {
            on_action(act);
        }
    });

    if let Err(e) = attach_res {
        log_error(&format!("failed to attach MediaControls handler: {e:?}"));
        return;
    }

    STATE.with(|cell| {
        *cell.borrow_mut() = Some(State {
            controls,
            last_meta: None,
            last_playback: None,
            is_stopped: false,
        });
    });
}

fn metadata<'a>(s: &'a MediaSnapshot, cover: &'a str) -> MediaMetadata<'a> {
    MediaMetadata {
        title: Some(s.title.as_str()),
        artist: Some(s.artist.as_str()),
        album: Some(s.album.as_str()),
        cover_url: Some(cover),
        duration: Some(Duration::from_secs(s.duration_secs as u64)),
    }
}

/// Push a playback/metadata snapshot to SMTC. Must be called on the UI thread.
/// Diffs against the last pushed state so WinRT is only updated on real changes.
pub fn update(s: MediaSnapshot) {
    STATE.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let Some(state) = borrow.as_mut() else {
            return;
        };

        if s.video_id.is_empty() || s.video_id == "native-prototype" {
            if !state.is_stopped {
                if let Err(e) = state.controls.set_playback(MediaPlayback::Stopped) {
                    log_error(&format!("set_playback(Stopped) failed: {e:?}"));
                } else {
                    state.is_stopped = true;
                    state.last_meta = None;
                    state.last_playback = None;
                }
            }
            return;
        }

        state.is_stopped = false;

        let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", s.video_id));
        let has_local_thumb = thumb_path.exists();

        let new_meta = PushedMeta {
            video_id: s.video_id.clone(),
            title: s.title.clone(),
            artist: s.artist.clone(),
            album: s.album.clone(),
            duration_secs: s.duration_secs,
            has_local_thumb,
        };

        let track_changed = state
            .last_meta
            .as_ref()
            .map(|m| m.video_id != s.video_id)
            .unwrap_or(true);

        if state.last_meta.as_ref() != Some(&new_meta) {
            let remote_cover = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", s.video_id);
            let cover_url = if has_local_thumb {
                format!("file://{}", thumb_path.display())
            } else {
                remote_cover.clone()
            };
            let mut result = state.controls.set_metadata(metadata(&s, &cover_url));
            if result.is_err() && has_local_thumb {
                result = state.controls.set_metadata(metadata(&s, &remote_cover));
            }
            if let Err(e) = result {
                log_error(&format!("set_metadata failed: {e:?}"));
            }
            // Recorded even on failure so a persistent error doesn't retry every tick.
            state.last_meta = Some(new_meta);
        }

        let now = Instant::now();
        let elapsed = s.elapsed_secs.max(0.0);
        let needs_playback_update = match &state.last_playback {
            None => true,
            Some(prev) => {
                let expected = if prev.is_playing {
                    prev.base_elapsed_secs + now.duration_since(prev.pushed_at).as_secs_f64()
                } else {
                    prev.base_elapsed_secs
                };
                track_changed
                    || prev.video_id != s.video_id
                    || prev.is_playing != s.is_playing
                    || (elapsed - expected).abs() > 2.0
            }
        };

        if needs_playback_update {
            let progress = Some(MediaPosition(Duration::from_secs_f64(elapsed)));
            let playback = if s.is_playing {
                MediaPlayback::Playing { progress }
            } else {
                MediaPlayback::Paused { progress }
            };

            if let Err(e) = state.controls.set_playback(playback) {
                log_error(&format!("set_playback failed: {e:?}"));
            } else {
                state.last_playback = Some(PushedPlayback {
                    video_id: s.video_id,
                    is_playing: s.is_playing,
                    base_elapsed_secs: elapsed,
                    pushed_at: now,
                });
            }
        }
    });
}
