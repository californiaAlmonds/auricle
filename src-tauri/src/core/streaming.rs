use rusty_ytdl::{Video, VideoFormat};

fn audio_bitrate_score(format: &VideoFormat) -> u64 {
    format
        .audio_bitrate
        .or(format.average_bitrate)
        .unwrap_or(format.bitrate)
}

fn pick_audio_format<'a>(formats: &'a [VideoFormat], quality: &str) -> Option<&'a VideoFormat> {
    let mut candidates: Vec<&VideoFormat> = formats
        .iter()
        .filter(|format| format.has_audio && !format.url.is_empty() && !format.is_live && !format.is_hls)
        .collect();

    if candidates.is_empty() {
        candidates = formats
            .iter()
            .filter(|format| format.has_audio && !format.url.is_empty() && !format.is_live)
            .collect();
    }

    if candidates.is_empty() {
        return None;
    }

    candidates.sort_by_key(|format| audio_bitrate_score(format));

    match quality {
        "low" => candidates.first().copied(),
        "high" => candidates.last().copied(),
        _ => {
            let middle_index = if candidates.len() == 1 {
                0
            } else {
                (candidates.len() - 1) / 2
            };

            candidates
                .get(middle_index)
                .copied()
                .or_else(|| candidates.last().copied())
        }
    }
}

fn pick_native_audio_format<'a>(formats: &'a [VideoFormat], quality: &str) -> Option<&'a VideoFormat> {
    let mut preferred: Vec<&VideoFormat> = formats
        .iter()
        .filter(|format| format.has_audio && !format.url.is_empty() && !format.is_live && !format.is_hls)
        .filter(|format| {
            let mime = format!("{:?}", format.mime_type).to_lowercase();
            mime.contains("audio/mp4")
                || mime.contains("mp4a")
                || mime.contains("audio/mpeg")
                || mime.contains("audio/mp3")
        })
        .collect();

    if preferred.is_empty() {
        preferred = formats
            .iter()
            .filter(|format| format.has_audio && !format.url.is_empty() && !format.is_live && !format.is_hls)
            .collect();
    }

    if preferred.is_empty() {
        return None;
    }

    preferred.sort_by_key(|format| audio_bitrate_score(format));

    match quality {
        "low" => preferred.first().copied(),
        "high" => preferred.last().copied(),
        _ => {
            let middle_index = if preferred.len() == 1 {
                0
            } else {
                (preferred.len() - 1) / 2
            };

            preferred
                .get(middle_index)
                .copied()
                .or_else(|| preferred.last().copied())
        }
    }
}

pub async fn resolve_stream_url(video_id: &str, quality: Option<&str>) -> Result<String, String> {
    let selected_quality = quality.unwrap_or("medium");
    let video_url = format!("https://www.youtube.com/watch?v={video_id}");

    let video = Video::new(&video_url)
        .map_err(|err| format!("Failed to initialize stream extractor: {err}"))?;

    let video_info = video
        .get_info()
        .await
        .map_err(|err| format!("Failed to fetch stream info: {err}"))?;

    let format = pick_audio_format(&video_info.formats, selected_quality)
        .ok_or_else(|| "No playable audio stream found for this track".to_string())?;

    log::info!(
        "Selected native audio stream for {} using {} quality (itag {}, audio bitrate {:?})",
        video_id,
        selected_quality,
        format.itag,
        format.audio_bitrate,
    );

    Ok(format.url.clone())
}

pub async fn resolve_stream_url_for_native_audio(video_id: &str, quality: Option<&str>) -> Result<String, String> {
    let selected_quality = quality.unwrap_or("medium");
    let video_url = format!("https://www.youtube.com/watch?v={video_id}");

    let video = Video::new(&video_url)
        .map_err(|err| format!("Failed to initialize stream extractor: {err}"))?;

    let video_info = video
        .get_info()
        .await
        .map_err(|err| format!("Failed to fetch stream info: {err}"))?;

    let format = pick_native_audio_format(&video_info.formats, selected_quality)
        .ok_or_else(|| "No native-decodable audio stream found for this track".to_string())?;

    log::info!(
        "Selected native playback stream for {} using {} quality (itag {}, mime {:?}, audio bitrate {:?})",
        video_id,
        selected_quality,
        format.itag,
        format.mime_type,
        format.audio_bitrate,
    );

    Ok(format.url.clone())
}
