#![forbid(unsafe_code)]
#![deny(warnings)]
#![deny(clippy::pedantic)]

mod args;
mod hls;
mod http;
mod player;
mod segment_worker;

use std::{thread, time::Instant};

use anyhow::Result;
use log::{debug, info, warn};
use simplelog::{
    format_description, ColorChoice, ConfigBuilder, LevelFilter, TermLogger, TerminalMode,
};

use args::Args;
use hls::{Error as HlsErr, MasterPlaylist, MediaPlaylist, PrefetchUrlKind};
use player::Player;
use segment_worker::{Error as WorkerErr, Worker};

enum Reason {
    Reset,
    Exit,
}

fn run(mut player: Player, mut playlist: MediaPlaylist, max_retries: u32) -> Result<Reason> {
    let mut worker = Worker::new(player.stdin())?;
    worker.send(playlist.urls.take(PrefetchUrlKind::Newest)?)?;
    worker.sync()?;

    let mut retry_count: u32 = 0;
    loop {
        let time = Instant::now();
        match playlist.reload() {
            Ok(()) => retry_count = 0,
            Err(e) => match e.downcast_ref::<HlsErr>() {
                Some(HlsErr::Unchanged | HlsErr::InvalidPrefetchUrl | HlsErr::InvalidDuration) => {
                    retry_count += 1;
                    if retry_count == max_retries {
                        info!("Maximum retries on media playlist reached, exiting...");
                        return Ok(Reason::Exit);
                    }

                    debug!("{e}, retrying...");
                    continue;
                }
                Some(HlsErr::Advertisement) => {
                    warn!("{e}, resetting...");
                    return Ok(Reason::Reset);
                }
                Some(HlsErr::Discontinuity) => {
                    warn!("{e}, stream may be broken");
                }
                _ => return Err(e),
            },
        }

        let next_url = playlist.urls.take(PrefetchUrlKind::Next)?;
        let newest_url = playlist.urls.take(PrefetchUrlKind::Newest)?;
        if next_url.host_str().unwrap() == newest_url.host_str().unwrap() {
            worker.send(next_url)?;
        } else {
            worker.send(next_url)?;

            debug!("Host changed, spawning new segment worker");
            worker = Worker::new(player.stdin())?;
            worker.send(newest_url)?;
            worker.sync()?;
        }

        if let Some(sleep_time) = playlist.duration.checked_sub(time.elapsed()) {
            thread::sleep(sleep_time);
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse()?;
    if args.debug {
        TermLogger::init(
            LevelFilter::Debug,
            ConfigBuilder::new()
                .set_time_format_custom(format_description!(
                    "[hour]:[minute]:[second].[subsecond digits:5]"
                ))
                .set_time_offset_to_local()
                .unwrap() //isn't an error
                .build(),
            TerminalMode::Stderr,
            ColorChoice::Auto,
        )?;
    } else {
        TermLogger::init(
            LevelFilter::Info,
            ConfigBuilder::new()
                .set_time_level(LevelFilter::Off)
                .build(),
            TerminalMode::Stderr,
            ColorChoice::Auto,
        )?;
    }
    debug!("{:?}", args);

    let master_playlist = MasterPlaylist::new(&args.servers)?;
    loop {
        let playlist_url = master_playlist.fetch_variant_playlist(&args.channel, &args.quality)?;
        if args.passthrough {
            println!("{playlist_url}");
            return Ok(());
        }

        let playlist = match MediaPlaylist::new(playlist_url) {
            Ok(playlist) => playlist,
            Err(e) => match e.downcast_ref::<HlsErr>() {
                Some(HlsErr::Advertisement | HlsErr::Discontinuity) => {
                    warn!("{e} on startup, resetting...");
                    continue;
                }
                Some(HlsErr::NotLowLatency(url)) => {
                    info!("{e}, opening player with playlist URL");
                    let player_args = args
                        .player_args
                        .split_whitespace()
                        .map(|s| if s == "-" { url.clone() } else { s.to_owned() })
                        .collect::<Vec<String>>()
                        .join(" ");

                    let mut player = Player::spawn(&args.player_path, &player_args)?;
                    player.wait()?;

                    return Ok(());
                }
                _ => return Err(e),
            },
        };

        let player = Player::spawn(&args.player_path, &args.player_args)?;
        match run(player, playlist, args.max_retries) {
            Ok(reason) => match reason {
                Reason::Reset => continue,
                Reason::Exit => return Ok(()),
            },
            Err(e) => match e.downcast_ref::<WorkerErr>() {
                Some(WorkerErr::SendFailed | WorkerErr::SyncFailed) => {
                    info!("Player closed, exiting...");
                    return Ok(());
                }
                _ => return Err(e),
            },
        }
    }
}
