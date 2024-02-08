use std::{mem, str::FromStr, thread, time::Duration as StdDuration, time::Instant};

use anyhow::{Context, Result};
use log::{debug, info};

use super::playlist::MediaPlaylist;
use crate::{http::Url, worker::Worker};

//Used for av1/hevc streams
pub struct Header(pub Option<Url>);

impl FromStr for Header {
    type Err = anyhow::Error;

    fn from_str(playlist: &str) -> Result<Self, Self::Err> {
        Ok(Self(
            playlist
                .lines()
                .find(|s| s.starts_with("#EXT-X-MAP"))
                .and_then(|s| s.split_once('='))
                .map(|s| s.1.replace('"', "").into()),
        ))
    }
}

#[derive(Default, Clone, PartialEq, Debug)]
pub struct Duration {
    pub is_ad: bool,
    pub was_capped: bool,
    inner: StdDuration,
}

impl FromStr for Duration {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self {
            is_ad: s.contains('|'),
            was_capped: bool::default(),
            inner: StdDuration::try_from_secs_f32(
                s.split_once(':')
                    .and_then(|s| s.1.split_once(','))
                    .map(|s| s.0.parse())
                    .context("Invalid segment duration")??,
            )
            .context("Failed to parse segment duration")?,
        })
    }
}

impl Duration {
    pub fn sleep(&mut self, elapsed: StdDuration) {
        //can't wait too long or the server will close the socket
        const MAX_DURATION: StdDuration = StdDuration::from_secs(3);
        if self.inner >= MAX_DURATION || self.was_capped {
            self.sleep_half(elapsed);

            self.was_capped = true;
            return;
        }

        Self::sleep_thread(self.inner, elapsed);
    }

    pub fn sleep_half(&self, elapsed: StdDuration) {
        if let Some(half) = self.inner.checked_div(2) {
            Self::sleep_thread(half, elapsed);
        }
    }

    fn sleep_thread(duration: StdDuration, elapsed: StdDuration) {
        if let Some(sleep_time) = duration.checked_sub(elapsed) {
            debug!("Sleeping thread for {:?}", sleep_time);
            thread::sleep(sleep_time);
        }
    }
}

#[derive(Default, Clone, Debug)]
pub enum Segment {
    Normal(Duration, Url),
    NextPrefetch(Duration, Url),
    NewestPrefetch(Url),

    #[default]
    Unknown,
}

impl PartialEq for Segment {
    fn eq(&self, other: &Self) -> bool {
        let (_, self_url) = self.destructure_ref();
        let (_, other_url) = other.destructure_ref();

        self_url == other_url
    }
}

impl Segment {
    pub fn find_next(&self, segments: &mut [Segment]) -> Option<Segment> {
        if let Some(idx) = segments.iter().position(|s| self == s) {
            let idx = idx + 1;
            if idx == segments.len() {
                return None;
            }

            //shouldn't panic, already did bounds check
            return Some(mem::take(&mut segments[idx]));
        }

        Some(Segment::Unknown)
    }

    pub fn destructure_ref(&self) -> (Option<&Duration>, Option<&Url>) {
        match self {
            Self::Normal(duration, url) | Self::NextPrefetch(duration, url) => {
                (Some(duration), Some(url))
            }
            Self::NewestPrefetch(url) => (None, Some(url)),
            Self::Unknown => (None, None),
        }
    }

    pub fn destructure_mut(&mut self) -> (Option<&mut Duration>, Option<&mut Url>) {
        match self {
            Self::Normal(ref mut duration, ref mut url)
            | Self::NextPrefetch(ref mut duration, ref mut url) => (Some(duration), Some(url)),

            Self::NewestPrefetch(ref mut url) => (None, Some(url)),
            Self::Unknown => (None, None),
        }
    }
}

pub struct Handler {
    playlist: MediaPlaylist,
    worker: Worker,
    prev_segment: Segment,
    init: bool,
}

impl Handler {
    pub fn new(playlist: MediaPlaylist, worker: Worker) -> Self {
        Self {
            playlist,
            worker,
            prev_segment: Segment::default(),
            init: true,
        }
    }

    pub fn reload(&mut self) -> Result<()> {
        self.playlist.reload()
    }

    pub fn process(&mut self, time: Instant) -> Result<()> {
        let mut segments = self.playlist.segments()?;
        let duration = segments.iter_mut().rev().find_map(|s| match s {
            Segment::Normal(duration, _) => Some(duration),
            _ => None,
        });

        if let Some(duration) = duration {
            if duration.is_ad {
                info!("Filtering ad segment...");
                duration.sleep(time.elapsed());

                return Ok(());
            }
        }

        if let Some(mut segment) = self.prev_segment.find_next(segments.as_mut_slice()) {
            debug!("Previous:\n{:?}", self.prev_segment);
            debug!("Next:\n{segment:?}\n");

            match segment {
                Segment::Normal(ref mut duration, ref mut url)
                | Segment::NextPrefetch(ref mut duration, ref mut url) => {
                    self.worker.url(url.take())?;
                    duration.sleep(time.elapsed());
                }
                Segment::NewestPrefetch(ref mut url) => self.worker.sync_url(url.take())?,
                Segment::Unknown => {
                    if !self.init {
                        info!("Failed to find next segment, skipping to newest...");
                    }
                    self.init = false;

                    let mut last_segment = segments
                        .into_iter()
                        .last()
                        .context("Failed to find newest segment")?;

                    let (_, url) = last_segment.destructure_mut();
                    self.worker.sync_url(
                        url.context("Failed to get last segment URL while skipping to newest")?
                            .take(),
                    )?;

                    self.prev_segment = last_segment;
                    return Ok(());
                }
            }

            self.prev_segment = segment;
        } else {
            let (duration, _) = self.prev_segment.destructure_ref();

            let duration = duration.context("Failed to get segment duration while retrying")?;
            if !duration.was_capped {
                info!("Playlist unchanged, retrying...");
            }

            duration.sleep_half(time.elapsed());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::playlist::tests::create_playlist;
    use super::super::tests::PLAYLIST;
    use super::*;

    #[test]
    fn parse_header() {
        assert_eq!(
            PLAYLIST.parse::<Header>().unwrap().0,
            Some("http://header.invalid".into()),
        );
    }

    #[test]
    fn parse_prefetch_segments() {
        let playlist = create_playlist();

        let segments = playlist.segments().unwrap();
        assert_eq!(
            segments.into_iter().last().unwrap(),
            Segment::NewestPrefetch("http://newest-prefetch-url.invalid".into()),
        );

        let segments = playlist.segments().unwrap();
        assert_eq!(
            segments[segments.len() - 2],
            Segment::NextPrefetch(
                Duration {
                    is_ad: false,
                    was_capped: false,
                    inner: StdDuration::from_secs_f32(0.978),
                },
                "http://next-prefetch-url.invalid".into(),
            ),
        );
    }
}
