use std::sync::OnceLock;

use super::playback::PlaybackCore;

static PLAYBACK_CORE: OnceLock<PlaybackCore> = OnceLock::new();

pub fn playback_core() -> &'static PlaybackCore {
    PLAYBACK_CORE.get_or_init(PlaybackCore::new)
}
