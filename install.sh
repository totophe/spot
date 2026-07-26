#!/bin/sh
# spot installer — download the latest release binary for this host.
#
#   curl -fsSL https://raw.githubusercontent.com/totophe/spot/main/install.sh | sh
#
# Honors:
#   SPOT_INSTALL_DIR   install location (default: ~/.local/bin)
#   SPOT_VERSION       tag to install   (default: latest)

set -eu

REPO="totophe/spot"
INSTALL_DIR="${SPOT_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${SPOT_VERSION:-latest}"

err() { printf '\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }
info() { printf '\033[36m%s\033[0m\n' "$1" >&2; }

need() { command -v "$1" >/dev/null 2>&1 || err "missing required command: $1"; }
need uname
command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 \
  || err "need curl or wget"

# --- detect target triple --------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  os_part="unknown-linux-gnu" ;;
  Darwin) os_part="apple-darwin" ;;
  *) err "unsupported OS: $os" ;;
esac
case "$arch" in
  x86_64|amd64)  arch_part="x86_64" ;;
  aarch64|arm64) arch_part="aarch64" ;;
  *) err "unsupported architecture: $arch" ;;
esac

# Say so plainly rather than letting the download 404: releases do not include
# Intel macOS, because that runner pool stalls badly enough to block them.
if [ "$os" = "Darwin" ] && [ "$arch_part" = "x86_64" ]; then
  err "Intel macOS has no published build — install a Rust toolchain and \`cargo build --release\`"
fi
asset="spot-${arch_part}-${os_part}"

# --- resolve version -------------------------------------------------------
fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1"
  else
    wget -qO- "$1"
  fi
}
download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$2" "$1"
  else
    wget -qO "$2" "$1"
  fi
}

if [ "$VERSION" = "latest" ]; then
  info "Resolving latest release…"
  VERSION="$(fetch "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep -o '"tag_name"[ ]*:[ ]*"[^"]*"' \
    | head -n1 \
    | sed 's/.*"tag_name"[ ]*:[ ]*"\([^"]*\)".*/\1/')"
  [ -n "$VERSION" ] || err "could not resolve latest release tag"
fi

url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
info "Installing spot ${VERSION} (${asset})…"

# --- download & install ----------------------------------------------------
mkdir -p "$INSTALL_DIR"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT INT TERM
download "$url" "$tmp" || err "download failed: $url"
[ -s "$tmp" ] || err "downloaded file is empty"
chmod +x "$tmp"
mv "$tmp" "$INSTALL_DIR/spot"
trap - EXIT INT TERM

# The `stay` symlink: spot dispatches on argv[0], so this makes the bare word
# `stay` work anywhere PATH reaches — including `:!stay` from inside an editor
# and inside a zellij pane.
ln -sf spot "$INSTALL_DIR/stay"

info "Installed to $INSTALL_DIR/spot (and $INSTALL_DIR/stay)"

# --- post-install hint -----------------------------------------------------
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) printf '\n\033[33mNote:\033[0m %s is not on your PATH. Add:\n  export PATH="%s:$PATH"\n' \
       "$INSTALL_DIR" "$INSTALL_DIR" >&2 ;;
esac

rc="$HOME/.bashrc"
case "${SHELL:-}" in
  *zsh) rc="$HOME/.zshrc" ;;
esac

cat >&2 <<EOF

To greet you with the session picker on every interactive login:

  spot --init >> "$rc"

Then reload:  source "$rc"

Run 'spot' now to try it, or 'spot --help' for options.
EOF
