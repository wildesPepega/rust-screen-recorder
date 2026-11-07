#!/usr/bin/env bash
# rust-screen-recorder installer
#
#   curl -fsSL https://raw.githubusercontent.com/wildesPepega/rust-screen-recorder/main/install.sh | bash
#
# Options (pass after `bash -s --`):
#   --uninstall      remove the program (your settings and clips are kept)
#   --no-start       install only, do not start the background recorder
#   --version vX.Y.Z install a specific release instead of the latest
#
# Installs to ~/.local/bin (override with PREFIX=/some/prefix). No root needed.
set -euo pipefail

REPO="wildesPepega/rust-screen-recorder"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
BIN="$BIN_DIR/rust-screen-recorder"
START=1
VERSION=""
ACTION="install"

while [ $# -gt 0 ]; do
    case "$1" in
        --uninstall) ACTION="uninstall" ;;
        --no-start) START=0 ;;
        --version) VERSION="${2:-}"; shift ;;
        -h|--help) sed -n '2,12p' "$0" 2>/dev/null || true; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 1 ;;
    esac
    shift
done

say()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

if [ "$ACTION" = "uninstall" ]; then
    if [ -x "$BIN" ]; then
        "$BIN" ctl quit >/dev/null 2>&1 || true
        "$BIN" uninstall || true
        rm -f "$BIN"
        say "Removed $BIN (settings in ~/.config/rust-screen-recorder and your clips were kept)."
    else
        warn "rust-screen-recorder is not installed in $BIN_DIR"
    fi
    exit 0
fi

# --- checks ---------------------------------------------------------------------------------
[ "$(uname -s)" = "Linux" ] || die "only Linux is supported"
[ "$(uname -m)" = "x86_64" ] || die "release builds are x86_64 only – please build from source"
for c in curl tar sha256sum; do command -v "$c" >/dev/null || die "'$c' is required"; done
command -v ffmpeg >/dev/null || warn "ffmpeg is not installed – recording will not work until you install it"
if ! { ldconfig -p 2>/dev/null || true; } | grep -q 'libavcodec.so.63'; then
    warn "FFmpeg 9 (libavcodec.so.63) was not found. The release binary needs it;"
    warn "on other FFmpeg versions build from source: https://github.com/$REPO#build-from-source"
fi
case "${XDG_CURRENT_DESKTOP:-}" in *KDE*) ;; *) warn "this program is made for KDE Plasma 6 (Wayland)" ;; esac

# --- find release -----------------------------------------------------------------------------
if [ -z "$VERSION" ]; then
    VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name"[^"]*"([^"]+)".*/\1/')" \
        || die "could not ask GitHub for the latest release"
fi
[ -n "$VERSION" ] || die "no release found"
NAME="rust-screen-recorder-$VERSION-x86_64-linux"
URL="https://github.com/$REPO/releases/download/$VERSION/$NAME.tar.gz"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
say "Downloading rust-screen-recorder $VERSION"
curl -fL --progress-bar -o "$TMP/$NAME.tar.gz" "$URL" || die "download failed: $URL"
curl -fsSL -o "$TMP/$NAME.tar.gz.sha256" "$URL.sha256" || die "checksum download failed"
(cd "$TMP" && sha256sum -c --quiet "$NAME.tar.gz.sha256") || die "checksum mismatch – aborting"
tar -xzf "$TMP/$NAME.tar.gz" -C "$TMP"
[ -f "$TMP/$NAME/rust-screen-recorder" ] || die "archive does not contain the binary"

# --- install ----------------------------------------------------------------------------------
if [ -x "$BIN" ]; then
    "$BIN" ctl quit >/dev/null 2>&1 && say "Stopped the running recorder" || true
fi
mkdir -p "$BIN_DIR"
install -m755 "$TMP/$NAME/rust-screen-recorder" "$BIN.new"
mv -f "$BIN.new" "$BIN"
say "Installed $BIN"

"$BIN" --help >/dev/null 2>&1 || die "the binary does not start on this system (missing libraries?) – build from source instead"

# Launcher entry: KWin grants portal-free screen capture based on it.
"$BIN" install >/dev/null && say "Installed the launcher entry"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR is not in your PATH – add it, or start the program from the application menu" ;;
esac

if [ "$START" = 1 ]; then
    "$BIN" >/dev/null 2>&1 || true
    say "rust-screen-recorder is running – press Alt+Z for the overlay."
else
    say "Done. Start it with: rust-screen-recorder"
fi
say "Updates are installed automatically at every start (can be turned off in Global Settings)."
