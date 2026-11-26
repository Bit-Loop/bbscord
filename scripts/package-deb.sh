#!/usr/bin/env bash
# Build a Debian / Ubuntu (.deb) package for bbscord (amd64 or arm64).
#
# Usage:
#   ./scripts/package-deb.sh <version> <deb-arch: amd64|arm64> <binary-path> <out-dir>

set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "Usage: $0 <version> <amd64|arm64> <binary-path> <out-dir>" >&2
  exit 2
fi

VERSION="$1"
DEB_ARCH="$2"
BIN_PATH="$3"
OUT_DIR="$4"

if [[ ! -f "$BIN_PATH" ]]; then
  echo "error: binary not found at $BIN_PATH" >&2
  exit 1
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

install -Dm755 "$BIN_PATH" "$STAGE/usr/bin/bbscord"
install -Dm644 "README.md" "$STAGE/usr/share/doc/bbscord/README.md"
install -Dm644 "LICENSE" "$STAGE/usr/share/doc/bbscord/copyright"

INSTALLED_KB="$(du -sk "$STAGE" | awk '{print $1}')"
mkdir -p "$STAGE/DEBIAN"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: bbscord
Version: ${VERSION}
Section: net
Priority: optional
Architecture: ${DEB_ARCH}
Installed-Size: ${INSTALLED_KB}
Maintainer: Bit-Loop <https://github.com/Bit-Loop/bbscord>
Homepage: https://github.com/Bit-Loop/bbscord
Depends: libc6, libasound2t64 | libasound2, libopus0, libgstreamer1.0-0, libgstreamer-plugins-base1.0-0, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-plugins-bad, gstreamer1.0-nice
Recommends: gstreamer1.0-libav, gstreamer1.0-plugins-ugly, mpv, wl-clipboard | xclip, btop
Description: Retro BBS-styled Rust Discord TUI client
 Terminal-native Discord client with DAVE MLS E2EE voice and Group DM calls,
 GStreamer WebRTC Go Live screensharing, animated GIF search, inline file and
 image attachments, btop-compatible themes, and dynamic 16x2 LCD to 4K layouts.
EOF

mkdir -p "$OUT_DIR"
OUT_DEB="${OUT_DIR}/bbscord_${VERSION}_${DEB_ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT_DEB"
echo "[package-deb] built $OUT_DEB"
dpkg-deb --info "$OUT_DEB"
