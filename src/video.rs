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
use crate::nav::Nav as _;
use crate::player::Choice;
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
        let mut refusals = Vec::new();
        // A signed-in session asks the YouTube Music player first, with a minted proof-of-origin
        // token, since the tokenless urls stop being served after their first megabyte.
        if self.is_cookie_auth() {
            match self.signed_video(video_id, max_height).await {
                Ok(opened) => return Ok(Some(opened)),
                Err(error) => {
                    log::debug!("video: the signed-in player failed {video_id}: {error:#}");
                    refusals.push(format!("{error:#}"));
                }
            }
        }
        // The embedded player next: its urls need no proof-of-origin token, and one it stops
        // serving partway through is renewed from the same client.
        match self.embedded_video(video_id, max_height).await {
            Ok(opened) => return Ok(Some(opened)),
            Err(error) => {
                log::debug!("video: the embedded player failed {video_id}: {error:#}");
                refusals.push(format!("{error:#}"));
            }
        }
        match self.guest_video(video_id, max_height).await {
            Ok(opened) => Ok(Some(opened)),
            Err(error) => {
                refusals.push(format!("{error:#}"));
                anyhow::bail!(
                    "no client serves the picture of {video_id}: {}",
                    refusals.join(", ")
                )
            }
        }
    }

    /// The signed-in YouTube Music player's picture of `video_id`, deciphered, with a minted
    /// proof-of-origin token on its url and already arriving. A url refused partway through is
    /// renewed from the same player with a fresh token.
    async fn signed_video(
        self: &Arc<Self>,
        video_id: &str,
        max_height: u32,
    ) -> Result<(VideoFormat, AudioStream)> {
        let client = Client::Music;
        let (chosen, solver) = self
            .ciphered_streams(video_id, client, Choice::Picture(max_height))
            .await?;
        let (width, height, fps) = (chosen.width, chosen.height, chosen.fps.max(1));
        let token = self.signed_token(video_id).await;
        let format = crate::player::signed(chosen, &solver, &token).await?;
        let itag = format.itag;
        let stream = AudioStream::open(self.http.clone(), &format)
            .await
            .with_context(|| format!("itag {itag} is not being served"))?
            .renewing(self, video_id, itag, client);
        Ok((
            VideoFormat {
                format,
                width,
                height,
                fps,
            },
            stream,
        ))
    }

    /// The embedded player's picture of `video_id`, deciphered and already arriving.
    async fn embedded_video(
        self: &Arc<Self>,
        video_id: &str,
        max_height: u32,
    ) -> Result<(VideoFormat, AudioStream)> {
        let client = Client::Embedded;
        let (chosen, solver) = self
            .ciphered_streams(video_id, client, Choice::Picture(max_height))
            .await?;
        let (width, height, fps) = (chosen.width, chosen.height, chosen.fps.max(1));
        let url = crate::player::decipher(&solver, &chosen).await?;
        let format = chosen.into_format(url, client);
        let itag = format.itag;
        let stream = AudioStream::open(self.http.clone(), &format)
            .await
            .with_context(|| format!("itag {itag} is not being served"))?
            .renewing(self, video_id, itag, client);
        Ok((
            VideoFormat {
                format,
                width,
                height,
                fps,
            },
            stream,
        ))
    }

    /// The guest client's picture of `video_id`, whose urls need no deciphering, already
    /// arriving.
    async fn guest_video(
        self: &Arc<Self>,
        video_id: &str,
        max_height: u32,
    ) -> Result<(VideoFormat, AudioStream)> {
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
        let itag = chosen.format.itag;
        let stream = AudioStream::open(self.http.clone(), &chosen.format)
            .await
            .with_context(|| format!("itag {itag} is not being served"))?
            .renewing(self, video_id, itag, Client::VisionOs);
        Ok((chosen, stream))
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

impl YtMusic {
    /// The other half of YouTube Music's Song/Video switch: the music video of a song, or the
    /// song of a music video. YouTube Music names it beside the track in a signed-in queue;
    /// where it does not, the half is looked for by title and artist. `None` when there is no
    /// other half, as for an upload with no song of its own.
    pub async fn counterpart(&self, video_id: &str) -> Result<Option<String>> {
        let response = self
            .execute(
                "next",
                Client::Music,
                serde_json::json!({
                    "videoId": video_id,
                    "isAudioOnly": true,
                    "enablePersistentPlaylistPanel": true,
                }),
            )
            .await?;
        for wrapper in crate::parse::find_renderers(&response, "playlistPanelVideoWrapperRenderer")
        {
            let primary =
                wrapper.str_at(&["primaryRenderer", "playlistPanelVideoRenderer", "videoId"]);
            if primary != Some(video_id) {
                continue;
            }
            let other = wrapper.items(&["counterpart"]).iter().find_map(|half| {
                half.str_at(&[
                    "counterpartRenderer",
                    "playlistPanelVideoRenderer",
                    "videoId",
                ])
            });
            if let Some(other) = other {
                log::debug!("video: youtube pairs {video_id} with {other}");
                return Ok(Some(other.to_owned()));
            }
        }
        let details = self
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
        let read = |key: &str| {
            details
                .pointer(&format!("/videoDetails/{key}"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let (kind, title, author) = (read("musicVideoType"), read("title"), read("author"));
        let query = format!("{} {}", crate::dedup::normalize_title(&title), author);
        let found = match kind.as_str() {
            "MUSIC_VIDEO_TYPE_ATV" => self.search_videos(&query).await?,
            "MUSIC_VIDEO_TYPE_OMV" => self.search_songs(&query).await?,
            _ => return Ok(None),
        };
        let other = found
            .into_iter()
            .filter(|candidate| candidate.video_id.as_deref() != Some(video_id))
            .filter(|candidate| match kind.as_str() {
                "MUSIC_VIDEO_TYPE_ATV" => candidate.is_video(),
                _ => !candidate.is_video(),
            })
            .find(|candidate| same_song(&title, &author, candidate))
            .and_then(|candidate| candidate.video_id);
        log::debug!("video: {video_id} ({kind}) is matched by title with {other:?}");
        Ok(other)
    }
}

/// Whether `candidate` is the song named `title` by `author` in its other form. A video's
/// title often leads with the artist, as `Artist - Title`, and its channel can be the artist's
/// or a label's, so the artist need only appear in one of them.
fn same_song(title: &str, author: &str, candidate: &crate::models::Track) -> bool {
    use crate::dedup::{normalize, normalize_title};
    let author = normalize(author);
    let bare = |text: &str| {
        let text = normalize_title(text);
        let artists = std::iter::once(author.clone()).chain(
            candidate
                .artists
                .iter()
                .map(|artist| normalize(&artist.name)),
        );
        artists
            .filter(|artist| !artist.is_empty())
            .find_map(|artist| text.strip_prefix(&artist).map(str::trim).map(str::to_owned))
            .filter(|rest| !rest.is_empty())
            .unwrap_or(text)
    };
    let titled = bare(title) == bare(&candidate.title);
    let credited = author.is_empty()
        || candidate.artists.iter().any(|artist| {
            let name = normalize(&artist.name);
            !name.is_empty() && (author.contains(&name) || name.contains(&author))
        })
        || normalize(&candidate.title).contains(&author);
    titled && credited
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
