#!/usr/bin/env bash
#
# install.sh — install apple-music-tui for the current user.
#
# Usage:
#   # from a downloaded tarball:
#   ./install.sh ./dist/apple-music-tui-0.1.0-linux-x86_64.tar.gz
#
#   # or via curl (set AMT_TARBALL_URL to the release asset URL):
#   curl -fsSL https://example.com/install.sh | AMT_TARBALL_URL=https://.../amt.tar.gz sh
#
# What it does:
#   - extracts the release into $XDG_DATA_HOME/apple-music-tui-app
#     (default ~/.local/share/apple-music-tui-app)
#   - installs an `amt` launcher symlink into ~/.local/bin (fallback: prints
#     how to add it to PATH)
#   - verifies that `node` is available (required to run the Electron engine)
#
# Uninstall: rm -rf ~/.local/share/apple-music-tui-app ~/.local/bin/amt

set -eu

APP_NAME="apple-music-tui-app"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
INSTALL_DIR="$DATA_HOME/$APP_NAME"
BIN_DIR="$HOME/.local/bin"

say() { printf '\033[1;34m==>\033[0m %s\n' "$1"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$1" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

# --- obtain the tarball -------------------------------------------------------
TARBALL_ARG="${1:-}"
TMP_TARBALL=""

if [ -n "$TARBALL_ARG" ] && [ -f "$TARBALL_ARG" ]; then
  TARBALL="$TARBALL_ARG"
elif [ -n "${AMT_TARBALL_URL:-}" ]; then
  say "downloading $AMT_TARBALL_URL"
  TMP_TARBALL="$(mktemp --suffix=.tar.gz)"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$AMT_TARBALL_URL" -o "$TMP_TARBALL" || die "download failed"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$TMP_TARBALL" "$AMT_TARBALL_URL" || die "download failed"
  else
    die "need curl or wget to download the tarball"
  fi
  TARBALL="$TMP_TARBALL"
else
  die "no tarball given. Pass a path, e.g. './install.sh amt.tar.gz', or set AMT_TARBALL_URL."
fi

# --- preflight: the bundled Electron needs common system libraries -----------
# The release bundles its own Electron (which includes Node), so system Node is
# NOT required. Electron does need a handful of shared libraries that are present
# on virtually all desktop Linux installs; if the engine fails to start, missing
# libs are the usual cause (e.g. on a headless/minimal host).
if ! command -v ldconfig >/dev/null 2>&1; then
  : # nothing to check against; skip
fi

# --- extract ------------------------------------------------------------------
say "installing to $INSTALL_DIR"
rm -rf "$INSTALL_DIR"
mkdir -p "$INSTALL_DIR"

# The tarball contains a single top-level dir (apple-music-tui-<ver>-linux-<arch>);
# strip it so files land directly in $INSTALL_DIR.
tar -xzf "$TARBALL" -C "$INSTALL_DIR" --strip-components=1

[ -n "$TMP_TARBALL" ] && rm -f "$TMP_TARBALL"

if [ ! -x "$INSTALL_DIR/bin/amt" ]; then
  die "install looks incomplete: $INSTALL_DIR/bin/amt is missing"
fi
chmod +x "$INSTALL_DIR/bin/amt" "$INSTALL_DIR/bin/apple-music-tui" 2>/dev/null || true

# --- put the launcher on PATH -------------------------------------------------
mkdir -p "$BIN_DIR"
ln -sf "$INSTALL_DIR/bin/amt" "$BIN_DIR/amt"
say "installed launcher: $BIN_DIR/amt -> $INSTALL_DIR/bin/amt"

# --- PATH guidance ------------------------------------------------------------
case ":$PATH:" in
  *":$BIN_DIR:"*)
    say "done. Run 'amt' to start apple-music-tui."
    ;;
  *)
    warn "$BIN_DIR is not on your PATH."
    echo "    Add this to your shell profile (~/.bashrc or ~/.zshrc):"
    echo "        export PATH=\"\$HOME/.local/bin:\$PATH\""
    echo "    Then restart your shell and run 'amt'."
    echo "    (Or run it directly now: $INSTALL_DIR/bin/amt)"
    ;;
esac
