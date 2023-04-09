//    Copyright (C) 2023 2bc4
//
//    This program is free software: you can redistribute it and/or modify
//    it under the terms of the GNU General Public License as published by
//    the Free Software Foundation, either version 3 of the License, or
//    (at your option) any later version.
//
//    This program is distributed in the hope that it will be useful,
//    but WITHOUT ANY WARRANTY; without even the implied warranty of
//    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//    GNU General Public License for more details.
//
//    You should have received a copy of the GNU General Public License
//    along with this program.  If not, see <https://www.gnu.org/licenses/>.

#![forbid(unsafe_code)]

use std::{
    cmp::{Ord, Ordering},
    io,
    io::Write,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use is_terminal::IsTerminal;
use log::{info, warn};
use simplelog::{ColorChoice, ConfigBuilder, LevelFilter, TermLogger, TerminalMode};

mod iothread;
mod playlist;
use iothread::IOThread;
use playlist::{MediaPlaylist, PlaylistError};

pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/111.0.0.0 Safari/537.36";

#[derive(Parser)]
#[command(version, next_line_help = true)]
struct Args {
    /// Playlist proxy server to fetch the playlist from.
    /// Can be multiple comma separated servers, will try each in order until successful.
    /// If URL path is "[ttvlol]" the playlist will be requested using the TTVLOL API.
    /// If URL includes "[channel]" it will be replaced with the channel argument at runtime.
    #[arg(short, long, value_name = "URL", verbatim_doc_comment)]
    server: String,

    /// Path to the player that the stream will be piped to,
    /// if not specified will write stream to stdout
    #[arg(short, long, value_name = "PATH")]
    player_path: Option<String>,

    /// Arguments to pass to the player
    #[arg(
        short = 'a',
        long,
        value_name = "ARGUMENTS",
        allow_hyphen_values = true
    )]
    player_args: Option<String>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    /// Twitch channel to watch (can also be twitch.tv/channel for Streamlink compatibility)
    channel: String,

    /// Stream quality/variant playlist to fetch (best, 1080p, 720p, 360p, 160p)
    quality: String,
}

fn spawn_player_or_stdout(
    player_path: &Option<String>,
    player_args: &str,
) -> Result<Box<dyn Write + Send>> {
    if let Some(player_path) = player_path {
        info!("Opening player: {} {}", player_path, player_args);
        Ok(Box::new(
            Command::new(player_path)
                .args(player_args.split_whitespace())
                .stdin(Stdio::piped())
                .spawn()
                .context("Failed to open player")?
                .stdin
                .take()
                .context("Failed to open player stdin")?,
        ))
    } else {
        ensure!(
            !io::stdout().is_terminal(),
            "No player set and stdout is a terminal, exiting..."
        );

        info!("Writing to stdout");
        Ok(Box::new(io::stdout()))
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.debug {
        TermLogger::init(
            LevelFilter::Debug,
            ConfigBuilder::new()
                .set_max_level(LevelFilter::Error)
                .set_time_level(LevelFilter::Error)
                .build(),
            TerminalMode::Stderr,
            ColorChoice::Auto,
        )?;
    } else {
        TermLogger::init(
            LevelFilter::Info,
            ConfigBuilder::new()
                .set_max_level(LevelFilter::Error)
                .set_time_level(LevelFilter::Off)
                .build(),
            TerminalMode::Stderr,
            ColorChoice::Never,
        )?;
    }

    let player_args = args.player_args.unwrap_or_default();
    loop {
        let io_thread = IOThread::new(spawn_player_or_stdout(&args.player_path, &player_args)?)?;
        let playlist = MediaPlaylist::new(&args.server, &args.channel, &args.quality)?;

        let mut segment = playlist.catch_up()?;
        io_thread.send_url(&segment.url)?;

        loop {
            let time = Instant::now();
            let prev_media_sequence = segment.media_sequence;

            segment = match playlist.reload() {
                Ok(segment) => segment,
                Err(e) => match e.downcast_ref::<PlaylistError>() {
                    Some(PlaylistError::NotFoundError) => {
                        info!("{}. Stream likely ended, exiting...", e);
                        return Ok(());
                    }
                    Some(PlaylistError::DiscontinuityError) => {
                        //TODO: Use fallback servers
                        warn!("{}. Restarting player and fetching a new playlist", e);
                        break;
                    }
                    _ => bail!(e),
                },
            };

            match segment.media_sequence.cmp(&prev_media_sequence) {
                Ordering::Greater => {
                    const SEGMENT_DURATION: Duration = Duration::from_secs(2);

                    io_thread.send_url(&segment.url)?;

                    let elapsed = time.elapsed();
                    if elapsed < SEGMENT_DURATION {
                        thread::sleep(SEGMENT_DURATION - elapsed);
                    } else {
                        warn!("Took longer than segment duration, stream may be broken");
                    }
                }
                Ordering::Equal => continue,
                Ordering::Less => bail!("Out of order media sequence"),
            }
        }
    }
}
