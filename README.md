# linux-player

A minimal music player for Linux, written in Rust.

> **Status: milestone M1 – playback core only.**
> The program currently plays a single audio file from the command line.
> There is no TUI, no playlist, no library and no metadata support yet.

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
audio file
    │
    ▼
symphonia   (demux + decode → interleaved f32 PCM)
    │
    ▼
bounded queue (VecDeque behind a Mutex)
    │
    ▼
cpal callback  (realtime thread)
    │
    ▼
default audio device
```

| Path                  | Responsibility                                            |
| --------------------- | --------------------------------------------------------- |
| `src/main.rs`         | CLI parsing, top level error reporting                     |
| `src/audio/decoder.rs`| open a file, probe the container, decode packets to PCM    |
| `src/audio/output.rs` | CPAL stream, sample queue, device/format negotiation       |
| `src/audio/player.rs` | playback loop, back-pressure between decoder and callback  |

The decoder never touches the audio device and the audio callback never touches
the file system; the two are connected by a bounded queue so the realtime thread
only ever performs a short memcpy.

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

## Roadmap

* **M1 (current)** – `audio file → decode → audio output`
* M2 – play / pause / stop, seek, volume control
* M3 – metadata, music library scanning, playlists
* M4 – Ratatui TUI, search, shuffle, repeat
