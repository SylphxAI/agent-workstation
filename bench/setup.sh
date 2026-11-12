#!/usr/bin/env bash
# Fetch everything the benchmark needs into a user-owned prefix (no root needed on
# Debian/Ubuntu: packages are unpacked with dpkg-deb -x, not installed).
#
#   ./setup.sh [PREFIX]        # default: ${XDG_CACHE_HOME:-$HOME/.cache}/wsbench
#   . PREFIX/env.sh            # exports WSBENCH_* paths for the harness
set -euo pipefail

PREFIX="${1:-${XDG_CACHE_HOME:-$HOME/.cache}/wsbench}"
FPC_VERSION="${FPC_VERSION:-150.0.7871.186}"
FPC_SHA256="${FPC_SHA256:-48eff860053efd9ad498d51370f0a1f59f983df39c02a72ad2a8682cdead4055}"
HERE="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$PREFIX"/{dl,debs}
cd "$PREFIX"

# Python env: harness deps + Camoufox browser binary.
uv venv -q -p 3.12 venv
VIRTUAL_ENV="$PREFIX/venv" uv pip install -q -e "$HERE"
venv/bin/python -m camoufox fetch

# fingerprint-chromium (pinned, checksum-verified).
FPC_TAR="ungoogled-chromium-${FPC_VERSION}-1-x86_64_linux.tar.xz"
if [ ! -x "fpc/${FPC_TAR%.tar.xz}/chrome" ]; then
  curl -fsSLo "dl/$FPC_TAR" \
    "https://github.com/adryfish/fingerprint-chromium/releases/download/${FPC_VERSION}/${FPC_TAR}"
  echo "$FPC_SHA256  dl/$FPC_TAR" | sha256sum -c -
  mkdir -p fpc && tar -xf "dl/$FPC_TAR" -C fpc
fi

# Google Chrome stable (reference engine).
if [ ! -x chrome/opt/google/chrome/google-chrome ]; then
  curl -fsSLo dl/chrome.deb https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb
  dpkg-deb -x dl/chrome.deb chrome
fi

# Compositor (sway, headless wlroots backend) and X11 fallback (openbox). Uses the
# system package if present, otherwise unpacks the distro packages and their
# missing libraries under $PREFIX/root.
if ! command -v sway >/dev/null || ! command -v openbox >/dev/null; then
  pkgs=$(apt-get -s install --no-install-recommends sway openbox 2>/dev/null | awk '/^Inst/{print $2}')
  (cd debs && apt-get download $pkgs)
  for d in debs/*.deb; do dpkg-deb -x "$d" root; done
fi

SWAY=$(command -v sway || echo "$PREFIX/root/usr/bin/sway")
OPENBOX=$(command -v openbox || echo "$PREFIX/root/usr/bin/openbox")
cat > env.sh <<EOF
export WSBENCH_SWAY=$SWAY
export WSBENCH_OPENBOX=$OPENBOX
export WSBENCH_CHROME=$PREFIX/chrome/opt/google/chrome/google-chrome
export WSBENCH_FPC=$PREFIX/fpc/${FPC_TAR%.tar.xz}/chrome
export PATH=$PREFIX/venv/bin:\$PATH
EOF
echo "ready: . $PREFIX/env.sh"
