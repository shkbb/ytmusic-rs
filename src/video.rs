//! The picture of a music video: its H.264 stream, read while it downloads.
//!
//! The sound of a video is played from its audio stream like any track's, so this only ever
//! picks a video-only stream. H.264 in fragmented MP4 is the one format every desktop can
//! decode in hardware without extra codecs, so it is the only one offered. The guest client
//! hands those urls out without a cipher or a proof-of-origin token.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde_json::Value;

use crate::client::YtMusic;
use crate::context::Client;
use crate::models::AudioFormat;
use crate::stream::AudioStream;

/// One video stream on offer, with the size of its picture.
#[derive(Clone, Debug)]
pub struct VideoFormat {
    /// The stream as the download machinery reads it. Its `loudness_db` is always `None`.
    pub format: AudioFormat,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl YtMusic {
    /// The H.264 picture of `video_id` no taller than `max_height`, or its smallest one when
    /// every stream is taller, already arriving. `None` for a song with no video of its own,
    /// such as an album track, whose picture YouTube serves as a still of its cover.
    pub async fn open_video(
        self: &Arc<Self>,
        video_id: &str,
        max_height: u32,
    ) -> Result<Option<(VideoFormat, AudioStream)>> {
        if !self.is_music_video(video_id).await? {
            return Ok(None);
        }
        let response = self.guest_player(video_id).await?;
        let offered: Vec<VideoFormat> = crate::player::playable(&response, video_id)?
            .iter()
            .filter_map(video_format)
            .collect();
        log::debug!(
            "video: {} offers {video_id} as {}",
            Client::VisionOs.name(),
            offered
                .iter()
                .map(|video| format!("{}@{}p", video.format.itag, video.height))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let chosen = choose(offered, max_height)
            .with_context(|| format!("{video_id} has no h.264 picture"))?;
        let stream = AudioStream::open(self.http.clone(), &chosen.format)
            .await
            .with_context(|| format!("itag {} is not being served", chosen.format.itag))?;
        Ok(Some((chosen, stream)))
    }

    /// Whether `video_id` is a video with a picture of its own: an official music video or an
    /// upload, rather than an album track. Only the YouTube Music player says which, and it
    /// says so even to a guest it will not stream to.
    pub async fn is_music_video(&self, video_id: &str) -> Result<bool> {
        let response = self
            .execute_with(
                "player",
                Client::Music,
                serde_json::json!({
                    "videoId": video_id,
                    "contentCheckOk": true,
                    "racyCheckOk": true,
                }),
                false,
            )
            .await?;
        let kind = response
            .pointer("/videoDetails/musicVideoType")
            .and_then(Value::as_str)
            .unwrap_or("MUSIC_VIDEO_TYPE_ATV");
        log::debug!("video: {video_id} is {kind}");
        Ok(kind != "MUSIC_VIDEO_TYPE_ATV")
    }
}

/// The tallest stream within `max_height`, or the shortest one when none is.
fn choose(offered: Vec<VideoFormat>, max_height: u32) -> Option<VideoFormat> {
    let (within, above): (Vec<_>, Vec<_>) = offered
        .into_iter()
        .partition(|video| video.height <= max_height);
    match within.is_empty() {
        true => above.into_iter().min_by_key(|video| video.height),
        false => within
            .into_iter()
            .max_by_key(|video| (video.height, video.fps)),
    }
}

/// A video-only H.264 stream in MP4 the player response offers with a plain url.
fn video_format(format: &Value) -> Option<VideoFormat> {
    let mime = format.get("mimeType").and_then(Value::as_str)?;
    if !mime.starts_with("video/mp4") || !mime.contains("avc1") {
        return None;
    }
    let number = |key: &str| format.get(key).and_then(Value::as_u64);
    let text = |key: &str| {
        format
            .get(key)
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok())
    };
    let codec = mime
        .split_once("codecs=\"")
        .map(|(_, tail)| tail.trim_end_matches('"'))
        .unwrap_or_default();
    Some(VideoFormat {
        format: AudioFormat {
            itag: number("itag")? as u32,
            url: format.get("url").and_then(Value::as_str)?.to_string(),
            mime: mime.to_string(),
            codec: codec.to_string(),
            bitrate: number("bitrate").unwrap_or(0) as u32,
            duration: text("approxDurationMs").map(std::time::Duration::from_millis),
            content_length: text("contentLength"),
            loudness_db: None,
            user_agent: Client::VisionOs.user_agent(),
        },
        width: number("width").unwrap_or(0) as u32,
        height: number("height")? as u32,
        fps: number("fps").unwrap_or(30) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn offer(itag: u64, height: u64, mime: &str) -> Value {
        json!({
            "itag": itag,
            "url": "https://example.invalid/",
            "mimeType": mime,
            "height": height,
            "width": height * 16 / 9,
            "fps": 25,
        })
    }

    #[test]
    fn only_h264_in_mp4_is_offered() {
        let offers = [
            offer(137, 1080, "video/mp4; codecs=\"avc1.640028\""),
            offer(248, 1080, "video/webm; codecs=\"vp9\""),
            offer(399, 1080, "video/mp4; codecs=\"av01.0.08M.08\""),
            offer(140, 0, "audio/mp4; codecs=\"mp4a.40.2\""),
        ];
        let found: Vec<u32> = offers
            .iter()
            .filter_map(video_format)
            .map(|video| video.format.itag)
            .collect();
        assert_eq!(found, [137]);
    }

    #[test]
    fn the_tallest_picture_within_the_limit_is_chosen() {
        let offers: Vec<VideoFormat> = [
            offer(137, 1080, "video/mp4; codecs=\"avc1.640028\""),
            offer(136, 720, "video/mp4; codecs=\"avc1.4D401F\""),
            offer(135, 480, "video/mp4; codecs=\"avc1.4D401E\""),
        ]
        .iter()
        .filter_map(video_format)
        .collect();
        assert_eq!(choose(offers.clone(), 720).unwrap().format.itag, 136);
        assert_eq!(choose(offers, 240).unwrap().format.itag, 135);
    }
}
