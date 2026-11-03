#!/usr/bin/env bash
#
# build-release.sh — assemble a self-contained apple-music-tui release tarball.
#
# The app is two runtimes: the Rust TUI binary and the Electron playback-engine
# (which bundles the castLabs Widevine Electron build in node_modules). This
# script produces a single tarball containing both plus a launcher, laid out so
# the installed TUI finds the engine via $AMT_ENGINE_DIR.
#
# Output: dist/apple-music-tui-<version>-linux-<arch>.tar.gz
#
# Layout inside the tarball (extracted to <prefix>/apple-music-tui-app/):
#   bin/apple-music-tui         the release TUI binary
#   bin/amt                     launcher that sets AMT_ENGINE_DIR and execs the binary
#   playback-engine/            engine sources + node_modules (Electron + Widevine)
#   VERSION
#
# Usage: scripts/build-release.sh
# Requires: cargo, node, npm.

set -euo pipefail

# --- locate the workspace root (this script lives in <root>/scripts) ----------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CRATE_DIR="$ROOT_DIR/apple-music-tui"
ENGINE_DIR="$ROOT_DIR/playback-engine"
DIST_DIR="$ROOT_DIR/dist"

# --- preflight ----------------------------------------------------------------
for tool in cargo node npm; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: required tool '$tool' is not installed" >&2
    exit 1
  fi
done

VERSION="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$CRATE_DIR/Cargo.toml" | head -1)"
VERSION="${VERSION:-0.0.0}"
ARCH="$(uname -m)"
PKG_NAME="apple-music-tui-${VERSION}-linux-${ARCH}"
STAGE_DIR="$DIST_DIR/$PKG_NAME"

echo "==> building apple-music-tui v$VERSION ($ARCH)"

# --- 1. build the release binary ----------------------------------------------
echo "==> cargo build --release"
( cd "$CRATE_DIR" && cargo build --release )
BIN="$CRATE_DIR/../target/release/apple-music-tui"
if [ ! -x "$BIN" ]; then
  # Workspace target dir may be at the root.
  BIN="$ROOT_DIR/target/release/apple-music-tui"
fi
if [ ! -x "$BIN" ]; then
  echo "error: release binary not found after build" >&2
  exit 1
fi

# --- 2. stage the layout ------------------------------------------------------
echo "==> staging into $STAGE_DIR"
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR/bin" "$STAGE_DIR/playback-engine"

cp "$BIN" "$STAGE_DIR/bin/apple-music-tui"
chmod +x "$STAGE_DIR/bin/apple-music-tui"

# Copy the engine sources (not node_modules yet, not tests, not logs).
cp "$ENGINE_DIR/package.json" "$STAGE_DIR/playback-engine/"
cp "$ENGINE_DIR/package-lock.json" "$STAGE_DIR/playback-engine/" 2>/dev/null || true
mkdir -p "$STAGE_DIR/playback-engine/src"
# Copy everything under src/ except *.test.js.
( cd "$ENGINE_DIR/src" && find . -type f ! -name '*.test.js' -print0 \
    | while IFS= read -r -d '' f; do
        mkdir -p "$STAGE_DIR/playback-engine/src/$(dirname "$f")"
        cp "$f" "$STAGE_DIR/playback-engine/src/$f"
      done )

# --- 3. install engine runtime deps (Electron + Widevine) into the stage ------
echo "==> installing engine runtime dependencies (downloads castLabs Electron ~230MB)"
(
  cd "$STAGE_DIR/playback-engine"
  # Electron is declared as a devDependency; a release still needs it at
  # runtime, so install all deps (npm ci honors package-lock for reproducibility
  # when present, else npm install).
  if [ -f package-lock.json ]; then
    npm ci --include=dev
  else
    npm install --include=dev
  fi
  # `npm ci` can skip materializing Electron's actual binary into this copy's
  # node_modules/electron/dist (it may reference a global cache). Force the
  # castLabs installer so the real binary is physically present in the tarball.
  if [ -f node_modules/electron/install.js ] && [ ! -x node_modules/electron/dist/electron ]; then
    echo "==> materializing the Electron binary into the staged copy"
    node node_modules/electron/install.js
  fi
  if [ ! -x node_modules/electron/dist/electron ]; then
    echo "error: Electron binary missing from staged engine; the tarball would not run" >&2
    exit 1
  fi
)

# --- 4. launcher --------------------------------------------------------------
cat > "$STAGE_DIR/bin/amt" <<'LAUNCHER'
#!/usr/bin/env bash
# Launcher for apple-music-tui: point the TUI at the bundled playback-engine,
# then exec the real binary. Resolves paths relative to this launcher so the
# install location can be anywhere.
set -euo pipefail
SELF="$(readlink -f "${BASH_SOURCE[0]}")"
APP_ROOT="$(cd "$(dirname "$SELF")/.." && pwd)"
export AMT_ENGINE_DIR="$APP_ROOT/playback-engine"
exec "$APP_ROOT/bin/apple-music-tui" "$@"
LAUNCHER
chmod +x "$STAGE_DIR/bin/amt"

echo "$VERSION" > "$STAGE_DIR/VERSION"

# --- 5. tarball ---------------------------------------------------------------
echo "==> creating tarball"
TARBALL="$DIST_DIR/${PKG_NAME}.tar.gz"
( cd "$DIST_DIR" && tar -czf "$TARBALL" "$PKG_NAME" )

echo "==> done"
echo "    tarball: $TARBALL"
echo "    size:    $(du -h "$TARBALL" | cut -f1)"
