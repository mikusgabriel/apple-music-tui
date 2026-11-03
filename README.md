# apple-music-tui

A lightweight, keyboard-driven Apple Music client for the Linux terminal.

It is split into two processes:

- **TUI** (`apple-music-tui/`) - a Rust/[ratatui](https://ratatui.rs) terminal app. It owns the UI, the Queue, search/library browsing, configuration, session state, auth tokens, and (optionally) MPRIS media-key integration.
- **Playback_Engine** (`playback-engine/`) - a disposable Electron/Chromium process that runs MusicKit JS for DRM-protected playback and hosts the Apple sign-in window. It is spawned and supervised by the TUI over a Unix-domain-socket JSON-RPC channel, and self-terminates if the TUI dies.

The TUI launches the engine for you; you do not run it by hand.

## Install (prebuilt release)

The easiest way to run apple-music-tui is the self-contained release tarball. It
bundles the TUI binary **and** the Electron playback engine (including the
Widevine-enabled runtime), so you do **not** need Rust, Node, or npm installed to
use it — only to build it.

Install from a release tarball:

```bash
# from a downloaded tarball
./scripts/install.sh apple-music-tui-<version>-linux-<arch>.tar.gz

# or over the network (point at the release asset)
curl -fsSL <install.sh-url> | AMT_TARBALL_URL=<tarball-url> sh
```

This installs into `~/.local/share/apple-music-tui-app/` and puts an `amt`
launcher in `~/.local/bin/`. If `~/.local/bin` is on your `PATH`, just run:

```bash
amt
```

Uninstall:

```bash
rm -rf ~/.local/share/apple-music-tui-app ~/.local/bin/amt
```

### Building the release tarball yourself

Requires `cargo`, `node`, and `npm` (build-time only). From the workspace root:

```bash
scripts/build-release.sh
```

This produces `dist/apple-music-tui-<version>-linux-<arch>.tar.gz` containing the
release binary, the engine with its `node_modules` (Electron + Widevine), and the
`amt` launcher. The launcher sets `AMT_ENGINE_DIR` so the binary finds the bundled
engine wherever it is installed.

## Running from source

The sections below describe running from a source checkout (for development).

## Requirements

- Linux (uses XDG paths, Unix domain sockets, and optionally a D-Bus session bus for MPRIS).
- A **Rust** toolchain - [install via rustup](https://rustup.rs). Edition 2021.
- **Node.js** and **npm**, for the Electron Playback_Engine.
- An **Apple Music subscription** - full-length playback requires signing in; without it you get 30-second previews.
- Optional: a running **D-Bus session bus** if you enable MPRIS, and an OS **secret service** (e.g. GNOME Keyring) if you use the `keyring` token backend.

> Note: full-length playback depends on Widevine/Chromium being available to Electron. Where the engine can't start, the TUI falls back to preview-only mode and shows a restart action.

## Setup

Clone the repo, then install the engine's dependencies:

```bash
cd playback-engine
npm install
cd ..
```

Build the TUI:

```bash
cargo build --release
```

## Running

From the workspace root:

```bash
cargo run --release
```

Or run the built binary directly (it locates the sibling `playback-engine/` relative to the workspace):

```bash
./target/release/apple-music-tui
```

On first run the TUI:

1. creates a default config file if one doesn't exist,
2. restores your previous session (Queue, volume, shuffle/repeat) if present,
3. spawns and connects the Playback_Engine,
4. opens the Apple sign-in window when no Music_User_Token is stored yet.

Sign in through that window once; the token is cached for next time.

Quit with `q` (or Ctrl-C). On quit the session is saved and the engine is shut down.

## Keybindings

Defaults (all overridable in the config - see below):

| Action | Key(s) |
| --- | --- |
| Move down / up | `j` / `k` (or `Down` / `Up`) |
| Open selected | `l` / `Right` / `Enter` |
| Back | `h` / `Left` |
| Search | `/` |
| Open Library | `g` then `l` |
| Open Queue | `g` then `q` |
| Play / pause | `Space` |
| Next / previous track | `n` / `p` |
| Seek forward / backward | `]` / `[` |
| Volume up / down | `+` / `-` |
| Toggle shuffle | `s` |
| Cycle repeat (off -> all -> one) | `r` |
| Refresh Library | `R` |
| Play next | `N` |
| Add to Queue | `a` |
| Remove from Queue | `x` |
| Move up / down in Queue | `K` / `J` |
| Sign out | `g` then `o` |
| Help overlay | `?` |
| Quit | `q` |

## Configuration

The config file is TOML at `$XDG_CONFIG_HOME/apple-music-tui/config.toml`, defaulting to `~/.config/apple-music-tui/config.toml`. It is created with defaults on first run; unknown keys are ignored with a warning, and a parse/validation error names the offending line and exits without touching the file.

Defaults:

```toml
storefront = "us"            # ISO 3166-1 alpha-2 country code
seek_step_seconds = 10       # 1..=300
mpris_enabled = false        # expose media keys over MPRIS/D-Bus
token_store_backend = "File" # "File" (0600) or "Keyring" (OS secret service)
```

Override any keybinding under a `[keybindings]` table, keyed by action name (`play_pause`, `next_track`, `volume_up`, `search`, `quit`, ...). Example:

```toml
[keybindings]
play_pause = "p"
quit = "ctrl+q"
```

Conflicting or unrecognized bindings are warned about at startup and fall back to their defaults.

## File locations

| What | Path |
| --- | --- |
| Config | `$XDG_CONFIG_HOME/apple-music-tui/config.toml` (default `~/.config/...`) |
| Session state | `$XDG_STATE_HOME/apple-music-tui/session.json` (default `~/.local/state/...`) |
| Log | `$XDG_STATE_HOME/apple-music-tui/app.log` |
| Tokens (File backend) | stored with `0600` permissions; or in the OS keyring with the Keyring backend |
| Engine IPC socket | `$XDG_RUNTIME_DIR/apple-music-tui/engine.sock` (falls back to a per-user path under the temp dir) |

## Development

Run the TUI test suite (unit + property + integration tests):

```bash
cargo test --workspace
```

Resource-footprint/profiling tests are ignored by default; run them explicitly:

```bash
cargo test -p apple-music-tui --test resource_footprint_profiling -- --ignored --nocapture
```

Run the Playback_Engine tests:

```bash
cd playback-engine
npm test
```
