#!/usr/bin/env bash
# Build an Arch Linux / Manjaro Pacman (.pkg.tar.zst) binary package from a compiled bbscord binary.
#
# Note: On x86_64 CI we build natively inside `archlinux:latest` via `makepkg`; this script
# also supports reproducible aarch64 (Arch Linux ARM / Manjaro ARM) packaging on arm64 runners.
#
# Usage:
#   ./scripts/package-arch.sh <version> <arch: x86_64|aarch64> <binary-path> <out-dir>

set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "Usage: $0 <version> <x86_64|aarch64> <binary-path> <out-dir>" >&2
  exit 2
fi

VERSION="$1"
PKG_ARCH="$2"
BIN_PATH="$3"
OUT_DIR="$4"

if [[ ! -f "$BIN_PATH" ]]; then
  echo "error: binary not found at $BIN_PATH" >&2
  exit 1
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

install -Dm755 "$BIN_PATH" "$STAGE/usr/bin/bbscord"
install -Dm644 "LICENSE" "$STAGE/usr/share/licenses/bbscord/LICENSE"
install -Dm644 "README.md" "$STAGE/usr/share/doc/bbscord/README.md"

PKG_SIZE="$(du -sb "$STAGE" | awk '{print $1}')"
BUILD_DATE="$(date -u +%s)"

cat > "$STAGE/.PKGINFO" <<EOF
pkgname = bbscord
pkgbase = bbscord
pkgver = ${VERSION}-1
pkgdesc = Retro BBS-styled Rust Discord TUI client with DAVE E2EE voice, Group DMs, WebRTC screensharing, animated GIFs, btop themes, and 16x2 LCD to 4K layouts
url = https://github.com/Bit-Loop/bbscord
builddate = ${BUILD_DATE}
packager = Bit-Loop CI <https://github.com/Bit-Loop/bbscord>
size = ${PKG_SIZE}
arch = ${PKG_ARCH}
license = MIT
license = Apache-2.0
depend = alsa-lib
depend = dbus
depend = gcc-libs
depend = glibc
depend = gst-plugins-bad
depend = gst-plugins-base
depend = gst-plugins-good
depend = gstreamer
depend = opus
optdepend = btop: system btop .theme palettes auto-discovered in /usr/share/btop/themes
optdepend = gst-libav: software H.264/VP8/VP9 fallback decoding for Go Live screenshares
optdepend = gst-plugins-ugly: x264enc software H.264 broadcasting fallback
optdepend = mpv: low-latency borderless pop-out window sink for Go Live screenshares
optdepend = pipewire: Wayland screen capture and low-latency audio routing
optdepend = wl-clipboard: Wayland clipboard copy and Ctrl+V image paste
optdepend = xclip: X11 clipboard copy and Ctrl+V image paste
optdepend = xdg-desktop-portal: Wayland/X11 portal monitor and window picker for Go Live
EOF

mkdir -p "$OUT_DIR"
OUT_PKG="$(cd "$OUT_DIR" && pwd)/bbscord-${VERSION}-1-${PKG_ARCH}.pkg.tar.zst"
(
  cd "$STAGE"
  tar --owner=0 --group=0 --zstd -cf "$OUT_PKG" .PKGINFO usr
)
echo "[package-arch] built $OUT_PKG"
tar --zstd -tf "$OUT_PKG"
