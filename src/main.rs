//! linux-player – a minimal Rust audio player for Linux.
//!
//! Milestone M1 only implements the playback core:
//!
//! ```text
//! audio file -> symphonia decode -> PCM buffer -> CPAL -> default device
//! ```
//!
//! Usage: `linux-player <audio-file>`

mod audio;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;

use crate::audio::Player;

/// Help text shown when the program is invoked without a file argument.
const USAGE: &str = "Usage: linux-player <audio-file>";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `{:#}` prints the whole anyhow context chain on a single line so
            // the user sees both the high level operation and the root cause.
            eprintln!("Error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Parses the command line, then plays the requested file to completion.
fn run() -> Result<()> {
    let path = match parse_args()? {
        Some(path) => path,
        // No arguments (or `-h`/`--help`): print usage and exit normally.
        None => {
            println!("{USAGE}");
            return Ok(());
        }
    };

    let mut player = Player::new();
    player.play(&path)
}

/// Returns `Ok(None)` when usage should be printed instead of playing a file.
fn parse_args() -> Result<Option<PathBuf>> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    match args.as_slice() {
        [] => Ok(None),
        [arg] if arg == "-h" || arg == "--help" => Ok(None),
        [arg] => Ok(Some(arg.clone())),
        // More than one positional argument: M1 plays exactly one file.
        _ => Ok(None),
    }
}
