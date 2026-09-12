# vvDOOM

Play Doom in [Vivido](../vivido/) through Vivid Protocol 1.5, with graphics and sound carried over
the same authenticated media stack used by Vivi. The game remains playable when its process runs
through `vvssh` or inside a Vivid-enabled vvmux pane; no Kitty graphics capability or audio device
is required on the machine running `vvdoom`.

Vvdoom is a Vivid-only port of Kitdoom. It sends Doom's 640×400 framebuffer as bounded live RGBA
raster frames and sends miniaudio's headless 48 kHz stereo mix as a separate live PCM track. Input
continues over the terminal path, including key releases and SGR pixel mouse motion.

## Usage

Run directly in Vivido:

```sh
cargo run --release
cargo run --release -- -nosound
cargo run --release -- -iwad /path/to/doom.wad
```

Vivido, `vvssh`, and vvmux provide these variables to the child process:

- `VIVID_ENDPOINT_CONTROL`
- optional `VIVID_ENDPOINT_REALTIME` and `VIVID_ENDPOINT_BULK`
- `VIVID_ROOT_SECRET` (consumed by the SDK and never accepted on the command line)

The endpoints can be overridden with `--control-endpoint`, `--realtime-endpoint`, and
`--bulk-endpoint`. Use `--asset-dir` or `VVDOOM_ASSET_DIR` to select a directory containing the WAD
and `sound/` files. All remaining arguments are passed to Doom.

For remote use, launch a normal shell with Vivido's `vvssh` and run `vvdoom` there. For vvmux,
start it from a Vivid-enabled Vivido session and run `vvdoom` in a pane; vvmux terminates the inner
Vivid session and re-originates its retained raster and audio tracks to Vivido.

## Controls

- Arrow keys or `W/J/K/L`: move and turn
- `A`/`D`: strafe
- `F`, `I`, or Control: fire
- Space: use
- `M`: toggle mouse input
- `U`: toggle fit-to-window and natural-size presentation
- Ctrl-C: quit and restore the terminal

`-nosound` omits the Vivid audio track. Vvdoom uses canonical `pcm_f32le`; if presenter audio fails
after startup, it removes only the audio track and keeps the game running silently. It never falls
back to an audio device on the producer host.

## Architecture

```text
doomgeneric C engine -> Rust FFI -> latest-wins RGBA queue -> Vivid bulk raster
miniaudio headless mixer -> bounded 20 ms PCM queue -> Vivid realtime audio
terminal keyboard/pixel mouse -> Doom event queue
```

Raster and audio have independent bounded workers. A congested SSH or vvmux media hop can discard
stale video or old queued audio without blocking Doom's simulation and input thread. Every raster
record is a full recovery frame, allowing Vivido or a terminating vvmux presenter to request and
retain a fresh composed framebuffer without a cross-hop delta dependency.

Doom draws twice per 35 Hz tic, so the raster path is shaped for latency rather than throughput: a
frame is submitted only when its pixels differ from the one before it, and the worker waits out the
declared frame period *before* taking a frame from the queue instead of after. Both keep the
channel's rate limiter from holding an already-stale image, which is what a keypress-to-picture
delay is made of. A motionless screen is re-sent once a second so a recovered channel is never left
blank.

The included `doom1.wad` is the Doom shareware episode. Supply a separately licensed WAD with
`-iwad` for other game data.

## Development

```sh
cargo fmt --all --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Vvdoom and its Doom-derived source are distributed under GPL-2.0-or-later. Vendored miniaudio is
available under its own public-domain or MIT-0 terms documented in its header.
