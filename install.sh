#!/bin/sh
# git-cognitive installer
#   curl -fsSL https://git-cognitive.cloud/install.sh | sh
#
# Downloads the latest prebuilt release binary for your platform and installs
# it into ~/.local/bin (override with GIT_COGNITIVE_INSTALL_DIR).

set -eu

REPO="ccherrad/git-cognitive"
BIN="git-cognitive"
INSTALL_DIR="${GIT_COGNITIVE_INSTALL_DIR:-$HOME/.local/bin}"

err() { printf 'error: %s\n' "$1" >&2; exit 1; }
info() { printf '%s\n' "$1" >&2; }

need() { command -v "$1" >/dev/null 2>&1 || err "missing required tool: $1"; }
need uname
need tar

if command -v curl >/dev/null 2>&1; then
  dl() { curl -fsSL "$1" -o "$2"; }
  fetch() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
  dl() { wget -qO "$2" "$1"; }
  fetch() { wget -qO- "$1"; }
else
  err "need curl or wget"
fi

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64)   target="x86_64-unknown-linux-gnu" ;;
      aarch64|arm64)  target="aarch64-unknown-linux-gnu" ;;
      *) err "unsupported Linux arch: $arch" ;;
    esac
    ;;
  Darwin)
    case "$arch" in
      arm64)   target="aarch64-apple-darwin" ;;
      x86_64)  target="x86_64-apple-darwin" ;;
      *) err "unsupported macOS arch: $arch" ;;
    esac
    ;;
  *)
    err "unsupported OS: $os (Windows: download the .zip from https://github.com/$REPO/releases)"
    ;;
esac

# Resolve the tag: honour GIT_COGNITIVE_VERSION, else follow the latest release.
tag="${GIT_COGNITIVE_VERSION:-}"
if [ -z "$tag" ]; then
  tag="$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
    | grep '"tag_name"' | head -n1 | cut -d'"' -f4)"
  [ -n "$tag" ] || err "could not determine latest release tag"
fi

asset="$BIN-$target.tar.gz"
url="https://github.com/$REPO/releases/download/$tag/$asset"

info "Installing $BIN $tag ($target)"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

dl "$url" "$tmp/$asset" || err "download failed: $url"
tar xzf "$tmp/$asset" -C "$tmp" || err "extract failed"

mkdir -p "$INSTALL_DIR"
mv "$tmp/$BIN" "$INSTALL_DIR/$BIN"
chmod +x "$INSTALL_DIR/$BIN"

info "Installed to $INSTALL_DIR/$BIN"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    info ""
    info "$INSTALL_DIR is not on your PATH. Add it:"
    info "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac

info ""
info "Get started:"
info "  git-cognitive enable claude   # install the post-commit hook"
info "  git-cognitive index           # index the current repo"
