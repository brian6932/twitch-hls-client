use std::{
    io::{self, Write},
    process::{Child, ChildStdin, Command, Stdio},
};

use anyhow::{Context, Result};
use log::{debug, error, info};
use url::Url;

#[derive(Clone, Debug)]
#[allow(clippy::struct_field_names)] //.args
pub struct Args {
    pub path: String,
    pub args: String,
    pub quiet: bool,
    pub no_kill: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            args: "-".to_owned(),
            path: String::default(),
            quiet: bool::default(),
            no_kill: bool::default(),
        }
    }
}

pub struct Player {
    stdin: ChildStdin,
    process: Child,
    no_kill: bool,
}

impl Drop for Player {
    fn drop(&mut self) {
        if !self.no_kill {
            if let Err(e) = self.process.kill() {
                error!("Failed to kill player: {e}");
            }
        }
    }
}

impl Write for Player {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        unimplemented!();
    }

    fn flush(&mut self) -> io::Result<()> {
        debug!("Finished writing segment");
        Ok(())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.stdin.write_all(buf)
    }
}

impl Player {
    pub fn spawn(args: &Args) -> Result<Self> {
        info!("Opening player: {} {}", args.path, args.args);
        let mut command = Command::new(&args.path);
        command
            .args(args.args.split_whitespace())
            .stdin(Stdio::piped());

        if args.quiet {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }

        let mut process = command.spawn().context("Failed to open player")?;
        let stdin = process
            .stdin
            .take()
            .context("Failed to open player stdin")?;

        Ok(Self {
            stdin,
            process,
            no_kill: args.no_kill,
        })
    }

    pub fn passthrough(pargs: &Args, url: &Url) -> Result<()> {
        info!("Passing through playlist URL to player");

        let mut pargs = pargs.to_owned();
        if pargs.args.split_whitespace().any(|a| a == "-") {
            pargs.args = pargs
                .args
                .split_whitespace()
                .map(|a| {
                    if a == "-" {
                        url.to_string()
                    } else {
                        a.to_owned()
                    }
                })
                .collect::<Vec<String>>()
                .join(" ");
        } else {
            pargs.args += &format!(" {url}");
        }

        let mut player = Self::spawn(&pargs)?;
        player
            .process
            .wait()
            .context("Failed to wait for player process")?;

        Ok(())
    }
}
