# linux-player

A minimal music player for Linux, written in Rust.

> **Status: milestone M2 – playback control.**
> The player plays one file at a time and can be paused, resumed and stopped.
> There is still no TUI, no playlist, no library and no metadata support.

```console
$ cargo run -- ~/Music/test.flac
Playing: test.flac
Format: FLAC
Sample rate: 44100 Hz
Channels: 2
Output device: default (44100 Hz, 2 channel(s), f32)
Press Ctrl+C to stop.
Playback finished.
```

## Architecture

```text
                    Player  (state machine, one session at a time)
                      │
        ┌─────────────┴───────────────┐
        ▼                             ▼
decode worker thread           AudioOutput (cpal stream)
  symphonia: demux + decode           │
        │  push                       │ callback (realtime thread)
        ▼                             ▼
     PcmBuffer  ─────────────────► default audio device
  (VecDeque + Mutex + Condvar)
```

| Path                   | Responsibility                                             |
| ---------------------- | ---------------------------------------------------------- |
| `src/main.rs`          | CLI parsing and output; the only place that prints          |
| `src/lib.rs`           | library root, exposes `audio`                               |
| `src/audio/decoder.rs` | open a file, probe the container, decode packets to PCM     |
| `src/audio/buffer.rs`  | PCM buffer: queue, pause/cancel flags, drain detection      |
| `src/audio/output.rs`  | CPAL stream creation, device/format negotiation             |
| `src/audio/player.rs`  | state machine, session lifecycle, decode worker             |
| `src/audio/state.rs`   | `PlaybackState`                                             |

The crate is split into a library and a binary: the playback core contains no CLI
code and never writes to stdout, so a future TUI can drive it unchanged.

Nothing that can block runs in the audio callback. The callback only copies
samples out of the buffer into the device buffer (converting them to the device's
sample format on the way); decoding, waiting for buffer space and waiting for the
device to drain all happen on the decode worker. Playing a file therefore returns
immediately, and `pause` / `resume` / `stop` just set a flag and return — they can
never be blocked by the audio thread.

### Playback states

| State      | Meaning                                                            |
| ---------- | ------------------------------------------------------------------ |
| `Stopped`  | nothing is playing: no session, no stream, no worker                |
| `Playing`  | audio is being produced; the worker is filling the buffer           |
| `Paused`   | the callback outputs silence and consumes nothing; position frozen  |
| `Finished` | the file ended naturally *and* every buffered sample was played     |

`Finished` and `Stopped` are deliberately different: `Finished` means the file
played to its end, `Stopped` means the user stopped playback (or nothing was
started yet). A decode or output failure is reported through
`Player::take_error()`, never as `Finished` and never swallowed.

### Control semantics

| Call                                      | Result                                    |
| ----------------------------------------- | ----------------------------------------- |
| `pause()` while `Playing`                 | `Paused`                                  |
| `pause()` while `Paused`                  | ok, no change (idempotent)                |
| `resume()` while `Paused`                 | `Playing`, from the paused position       |
| `resume()` while `Playing`                | ok, no change (idempotent)                |
| `pause()` / `resume()` while `Stopped`    | error: nothing is playing                 |
| `pause()` / `resume()` while `Finished`   | error: playback has already finished      |
| `stop()` in any state                     | `Stopped`; idempotent when already stopped |
| `play()` while something is playing       | old session is torn down, then the new one |
| `play()` with an unreadable file          | error; current playback keeps running     |

A pause keeps the samples that have not been played yet in the buffer and stops
consuming them, so the playback position cannot move and resuming continues from
exactly where it stopped. The buffer is bounded (about one second), which is what
keeps memory flat while paused and bounds how far the decoder may run ahead.

## Supported formats

| Format | Status                                             |
| ------ | -------------------------------------------------- |
| FLAC   | supported                                          |
| MP3    | supported                                          |
| WAV    | supported (comes for free with Symphonia)          |
| OGG    | supported (comes for free with Symphonia)          |

Any other container/codec pair that Symphonia can demux is likely to work too,
as long as the codec outputs PCM samples.

## System requirements

Decoding needs no external libraries, but the audio backend links against ALSA,
so the ALSA development headers are required at build time:

```console
# Debian / Ubuntu
$ sudo apt install libasound2-dev pkg-config

# Fedora
$ sudo dnf install alsa-lib-devel pkgconf-pkg-config

# Arch
$ sudo pacman -S alsa-lib pkgconf
```

No extra runtime packages are needed: on systems using PipeWire or PulseAudio,
the ALSA compatibility layer routes the stream to the running sound server, and
`cpal::default_host()` always selects the platform default device.

## Build and run

```console
$ cargo build --release
$ cargo run -- ~/Music/test.flac
```

Without an argument the usage line is printed and the program exits:

```console
$ cargo run
Usage: linux-player <audio-file>
```

## Tests

```console
$ cargo test                  # state machine; needs no sound card
$ cargo test -- --ignored     # opt-in: uses the real device and makes sound
```

The state machine tests drive the PCM buffer with a stand-in device that calls
exactly the function the CPAL callback calls, only with its own clock. That makes
start / pause / resume / stop, "a pause does not advance the position", "resuming
loses no sample" and "a new file never plays samples of the old one" verifiable
without a sound card and without making noise.

That is not the same as playing audio. The two `#[ignore]`d tests open the real
device and produce audible sound; they are what actually covers CPAL integration
and confirms that a pause really silences the output.

## Roadmap

* **M1** – `audio file → decode → audio output`
* **M2 (current)** – play / pause / resume / stop, state machine, clean teardown
* M3 – metadata, music library scanning, playlists, next/previous, seek, volume
* M4 – Ratatui TUI, search, shuffle, repeat
