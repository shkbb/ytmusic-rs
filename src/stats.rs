//! Watch reports: the pings YouTube's own player sends while a video plays, which put the play
//! in the account's history and count its watch time.
//!
//! The signed-in player response names two stats urls for the video. The playback url is sent
//! once, when the watch starts. The watchtime url is sent as the watch goes on, each time with
//! the stretches heard since the one before, and once more at the end. Every report of one
//! watch carries the same client playback nonce (`cpn`), which is what ties them together.

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::client::YtMusic;
use crate::context::{Client, random_string};

/// The host the player response names for its stats urls.
const STATS_HOST: &str = "https://s.youtube.com/";
/// The host the YouTube Music page sends them to, where the session's cookies apply as a
/// first party.
const MUSIC_HOST: &str = "https://music.youtube.com/";

/// Where to report one watch of a video, and the nonce every report of it carries.
#[derive(Clone, Debug)]
pub struct Tracking {
    playback: String,
    watchtime: String,
    cpn: String,
    hl: String,
    gl: String,
}

/// What one watchtime report says. Positions and times are in seconds.
#[derive(Clone, Copy, Debug)]
pub struct Watched<'a> {
    /// The stretches of the video heard since the last report, each as start and end.
    pub heard: &'a [(f64, f64)],
    /// Where playback is now.
    pub position: f64,
    /// How long ago the watch began, by the wall clock.
    pub elapsed: f64,
    pub paused: bool,
    /// Whether this is the last report of the watch, sent when it stops or moves on.
    pub last: bool,
}

impl Tracking {
    /// The client playback nonce of this watch.
    pub fn cpn(&self) -> &str {
        &self.cpn
    }

    /// The report that a watch began at `position`, `elapsed` seconds after it was set up.
    pub fn started(&self, position: f64, elapsed: f64) -> Result<String> {
        let mut url = self.base(&self.playback)?;
        url.query_pairs_mut()
            .append_pair("cmt", &seconds(position))
            .append_pair("rt", &seconds(elapsed))
            .append_pair("rtn", &seconds(elapsed))
            .append_pair("lact", &millis(elapsed));
        Ok(url.into())
    }

    /// The report of what was heard since the last one.
    pub fn watched(&self, watched: &Watched) -> Result<String> {
        let mut url = self.base(&self.watchtime)?;
        let list = |pick: fn(&(f64, f64)) -> f64| {
            watched
                .heard
                .iter()
                .map(|stretch| seconds(pick(stretch)))
                .collect::<Vec<_>>()
                .join(",")
        };
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("cmt", &seconds(watched.position))
                .append_pair("rt", &seconds(watched.elapsed))
                .append_pair("rtn", &seconds(watched.elapsed))
                .append_pair("lact", &millis(watched.elapsed))
                .append_pair(
                    "state",
                    match watched.paused {
                        true => "paused",
                        false => "playing",
                    },
                )
                .append_pair("st", &list(|stretch| stretch.0))
                .append_pair("et", &list(|stretch| stretch.1));
            if watched.last {
                query.append_pair("final", "1");
            }
        }
        Ok(url.into())
    }

    /// A stats url with the fields every report of this watch shares, as the YouTube Music
    /// page fills them in.
    fn base(&self, url: &str) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(url).context("the stats url does not parse")?;
        let client = Client::Music;
        url.query_pairs_mut()
            .append_pair("ver", "2")
            .append_pair("cpn", &self.cpn)
            .append_pair("c", client.name())
            .append_pair("cver", client.version())
            .append_pair("cplayer", "UNIPLAYER")
            .append_pair("cbr", "Chrome")
            .append_pair("cbrver", "125.0.0.0")
            .append_pair("cos", "Windows")
            .append_pair("cosver", "10.0")
            .append_pair("cplatform", "DESKTOP")
            .append_pair("hl", &self.hl)
            .append_pair("cr", &self.gl)
            .append_pair("volume", "100")
            .append_pair("muted", "0");
        Ok(url)
    }
}

impl YtMusic {
    /// Asks the signed-in YouTube Music player where to report a watch of `video_id`, under a
    /// fresh client playback nonce. Fails for a guest, for a video YouTube will not play to
    /// this account, and when the response names no stats urls.
    pub async fn playback_tracking(&self, video_id: &str) -> Result<Tracking> {
        if !self.is_cookie_auth() {
            bail!("only a signed-in session has plays to report");
        }
        let cpn = random_string(16);
        let payload = json!({
            "videoId": video_id,
            "racyCheckOk": true,
            "contentCheckOk": true,
            "cpn": cpn,
            "playbackContext": {
                "contentPlaybackContext": {
                    "html5Preference": "HTML5_PREF_WANTS",
                    "vis": 0,
                    "splay": false,
                    "lactMilliseconds": "-1",
                }
            },
        });
        let response = self
            .execute_with("player", Client::Music, payload, true)
            .await?;
        let status = response
            .pointer("/playabilityStatus/status")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN");
        if status != "OK" {
            bail!("youtube will not play {video_id} to this account ({status})");
        }
        let url = |key: &str| {
            response
                .pointer(&format!("/playbackTracking/{key}/baseUrl"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .with_context(|| format!("the player response for {video_id} has no {key}"))
        };
        Ok(Tracking {
            playback: url("videostatsPlaybackUrl")?,
            watchtime: url("videostatsWatchtimeUrl")?,
            cpn,
            hl: self.lang().to_string(),
            gl: self.region().to_string(),
        })
    }
}

/// `url` moved from the stats host to the YouTube Music one, where the page sends it.
pub(crate) fn on_music_host(url: &str) -> String {
    match url.strip_prefix(STATS_HOST) {
        Some(rest) => format!("{MUSIC_HOST}{rest}"),
        None => url.to_string(),
    }
}

/// Which report a stats url is, for a log line that leaves the query out.
pub(crate) fn kind(url: &str) -> &'static str {
    match url {
        url if url.contains("/api/stats/playback") => "playback",
        url if url.contains("/api/stats/watchtime") => "watchtime",
        _ => "stats",
    }
}

/// Seconds the way the player writes them, to the millisecond.
fn seconds(value: f64) -> String {
    format!("{:.3}", value.max(0.))
}

/// Seconds as whole milliseconds.
fn millis(value: f64) -> String {
    format!("{}", (value.max(0.) * 1000.).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracking() -> Tracking {
        Tracking {
            playback: "https://s.youtube.com/api/stats/playback?docid=abc&len=200".into(),
            watchtime: "https://s.youtube.com/api/stats/watchtime?docid=abc&len=200".into(),
            cpn: "0123456789abcdef".into(),
            hl: "en".into(),
            gl: "US".into(),
        }
    }

    #[test]
    fn a_stats_url_moves_to_the_music_host() {
        assert_eq!(
            on_music_host("https://s.youtube.com/api/stats/playback?a=1"),
            "https://music.youtube.com/api/stats/playback?a=1"
        );
    }

    #[test]
    fn a_watchtime_report_lists_every_stretch_heard() {
        let url = tracking()
            .watched(&Watched {
                heard: &[(0., 10.), (40., 52.5)],
                position: 52.5,
                elapsed: 23.,
                paused: false,
                last: true,
            })
            .unwrap();
        assert!(url.contains("docid=abc"));
        assert!(url.contains("cpn=0123456789abcdef"));
        assert!(url.contains("st=0.000%2C40.000"));
        assert!(url.contains("et=10.000%2C52.500"));
        assert!(url.contains("state=playing"));
        assert!(url.contains("final=1"));
    }
}
