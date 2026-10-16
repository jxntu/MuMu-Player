//! linux-player – a minimal Rust audio player for Linux.
//!
//! The command line front end of the `linux-player` library, whose playback core
//! lives in `linux_player::audio`:
//!
//! ```text
//! audio file -> symphonia decode -> PCM buffer -> cpal -> default device
//! ```
//!
//! Usage: `linux-player <audio-file>`

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;

use linux_player::audio::{PlaybackState, Player};

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

/// Parses the command line, then plays the requested file to the end.
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
    player.play(&path)?;

    // The player itself never writes to stdout: everything the CLI shows is
    // rendered here, which keeps the playback core usable from a future TUI.
    if let Some(info) = player.track_info() {
        println!("Playing: {}", display_name(&path));
        println!("Format: {}", info.format);
        println!("Sample rate: {} Hz", info.sample_rate);
        println!("Channels: {}", info.channels);
        if let Some(bits) = info.bits_per_sample {
            println!("Bit depth: {bits} bit");
        }
    }
    if let Some(info) = player.output_info() {
        println!("Output device: {info}");
    }
    println!("Press Ctrl+C to stop.");

    // Block until the session ends: either the file finished, or the decode
    // worker reported why it could not continue.
    let state = player.wait();
    if let Some(error) = player.take_error() {
        return Err(error);
    }
    if state == PlaybackState::Finished {
        println!("Playback finished.");
    }
    Ok(())
}

/// Returns `Ok(None)` when usage should be printed instead of playing a file.
fn parse_args() -> Result<Option<PathBuf>> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    match args.as_slice() {
        [] => Ok(None),
        [arg] if arg == "-h" || arg == "--help" => Ok(None),
        [arg] => Ok(Some(arg.clone())),
        // More than one positional argument: M2 still plays exactly one file.
        _ => Ok(None),
    }
}

/// File name of `path`, or the full display path if it does not have one.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
