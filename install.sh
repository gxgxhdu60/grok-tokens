#!/bin/sh
#
# grok-tokens installer — download a native binary from GitHub Releases.
# Other computers need only curl/wget + tar (no Rust, no git clone).
#
# Linux / macOS / WSL:
#   curl -fsSL https://github.com/gxgxhdu60/grok-tokens/releases/latest/download/install.sh | sh
#
set -eu

REPO_SLUG="${GROK_TOKENS_REPO:-gxgxhdu60/grok-tokens}"
INSTALL_DIR="${GROK_TOKENS_INSTALL_DIR:-${HOME}/.local/bin}"
BINARY_NAME="grok-tokens"
DEST="${INSTALL_DIR}/${BINARY_NAME}"

echo "Installing grok-tokens..."

os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)

# Piped through curl|sh: $0 is sh/bash. A real file next to Cargo.toml is a checkout.
is_piped() {
  case "$0" in
    sh|bash|dash|ash|zsh|-sh|-bash|-dash|-ash|-zsh|*/sh|*/bash|*/dash|*/ash|*/zsh)
      return 0
      ;;
  esac
  [ ! -f "$0" ]
}

need_downloader() {
  if command -v curl >/dev/null 2>&1; then
    return 0
  fi
  if command -v wget >/dev/null 2>&1; then
    return 0
  fi
  echo "Error: curl or wget is required."
  exit 1
}

fetch() {
  url=$1
  dest=$2
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --connect-timeout 20 --retry 2 -o "$dest" "$url"
  else
    wget -q -T 20 -O "$dest" "$url"
  fi
}

looks_like_binary() {
  path=$1
  [ -s "$path" ] || return 1
  hex=$(dd if="$path" bs=4 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')
  case "$hex" in
    7f454c46*|4d5a*|cffaedfe*|feedfacf*|cafebabe*|cefaedfe*|feedface*) return 0 ;;
  esac
  return 1
}

install_file() {
  src=$1
  mkdir -p "$INSTALL_DIR"
  tmp="${DEST}.tmp.$$"
  cp "$src" "$tmp"
  chmod 0755 "$tmp"
  mv "$tmp" "$DEST"
  echo "Installed: ${DEST}"
}

ensure_path() {
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) return 0 ;;
  esac

  marker='export PATH="$HOME/.local/bin:$PATH"'
  rc=""
  if [ -n "${ZSH_VERSION:-}" ]; then
    rc="${HOME}/.zshrc"
  elif [ -n "${BASH_VERSION:-}" ]; then
    if [ -f "${HOME}/.bashrc" ]; then
      rc="${HOME}/.bashrc"
    else
      rc="${HOME}/.bash_profile"
    fi
  elif [ -f "${HOME}/.zshrc" ]; then
    rc="${HOME}/.zshrc"
  elif [ -f "${HOME}/.bashrc" ]; then
    rc="${HOME}/.bashrc"
  else
    rc="${HOME}/.profile"
  fi

  if [ -f "$rc" ] && grep -F '.local/bin' "$rc" >/dev/null 2>&1; then
    :
  else
    {
      echo ""
      echo "# grok-tokens"
      echo "$marker"
    } >> "$rc"
    echo "Added ~/.local/bin to PATH in ${rc}"
  fi
}

finish() {
  ensure_path
  echo ""
  echo "If grok-tokens is not found:  export PATH=\"\$HOME/.local/bin:\$PATH\""
  echo "Verify:  grok-tokens --version && grok-tokens daily"
}

extract_and_install() {
  archive=$1
  extract_dir=$2
  rm -rf "$extract_dir"
  mkdir -p "$extract_dir"

  if tar -xzf "$archive" -C "$extract_dir" 2>/dev/null; then
    if [ -f "${extract_dir}/${BINARY_NAME}" ]; then
      DEST="${INSTALL_DIR}/${BINARY_NAME}"
      install_file "${extract_dir}/${BINARY_NAME}"
      return 0
    fi
    if [ -f "${extract_dir}/${BINARY_NAME}.exe" ]; then
      DEST="${INSTALL_DIR}/${BINARY_NAME}.exe"
      install_file "${extract_dir}/${BINARY_NAME}.exe"
      return 0
    fi
  fi

  if looks_like_binary "$archive"; then
    install_file "$archive"
    return 0
  fi
  return 1
}

try_asset() {
  name=$1
  tmp_dir=$2
  archive="${tmp_dir}/download.bin"
  extract_dir="${tmp_dir}/extract"
  url_file="${tmp_dir}/urls"

  {
    echo "https://github.com/${REPO_SLUG}/releases/latest/download/${name}"
    echo "https://ghfast.top/https://github.com/${REPO_SLUG}/releases/latest/download/${name}"
    echo "https://ghproxy.net/https://github.com/${REPO_SLUG}/releases/latest/download/${name}"
    echo "https://mirror.ghproxy.com/https://github.com/${REPO_SLUG}/releases/latest/download/${name}"
  } > "$url_file"

  while IFS= read -r url; do
    [ -n "$url" ] || continue
    echo "Trying ${url} ..."
    if fetch "$url" "$archive" && extract_and_install "$archive" "$extract_dir"; then
      return 0
    fi
  done < "$url_file"
  return 1
}

# ── Optional: local git checkout (developers) ────────────────────────────
SCRIPT_DIR=""
if ! is_piped && [ -f "$0" ]; then
  SCRIPT_DIR=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
fi

if [ -n "${SCRIPT_DIR}" ] \
  && [ -f "${SCRIPT_DIR}/Cargo.toml" ] \
  && [ -z "${GROK_TOKENS_FORCE_DOWNLOAD:-}" ]; then
  echo "Local checkout detected: ${SCRIPT_DIR}"
  if [ -x "${SCRIPT_DIR}/target/release/${BINARY_NAME}" ]; then
    install_file "${SCRIPT_DIR}/target/release/${BINARY_NAME}"
    finish
    exit 0
  elif command -v cargo >/dev/null 2>&1; then
    echo "Building release binary with cargo..."
    (cd "${SCRIPT_DIR}" && cargo build --release)
    install_file "${SCRIPT_DIR}/target/release/${BINARY_NAME}"
    finish
    exit 0
  else
    echo "No cargo / local binary — downloading GitHub Release."
  fi
fi

# ── GitHub Release binary ────────────────────────────────────────────────
need_downloader

TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/grok-tokens.XXXXXX")
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

download_ok=0

try_targets() {
  for target in "$@"; do
    for name in "grok-tokens-${target}.tar.gz" "grok-tokens-${target}"; do
      if try_asset "$name" "$TMP_DIR"; then
        download_ok=1
        return 0
      fi
    done
  done
  return 1
}

case "${os}-${arch}" in
  linux-x86_64|linux-amd64)
    try_targets x86_64-unknown-linux-musl x86_64-unknown-linux-gnu || true
    ;;
  linux-aarch64|linux-arm64)
    try_targets aarch64-unknown-linux-musl aarch64-unknown-linux-gnu || true
    ;;
  darwin-arm64|darwin-aarch64)
    try_targets aarch64-apple-darwin || true
    ;;
  darwin-x86_64)
    try_targets x86_64-apple-darwin || true
    ;;
  mingw*|msys*|cygwin*|*windows*)
    DEST="${INSTALL_DIR}/${BINARY_NAME}.exe"
    try_targets x86_64-pc-windows-msvc x86_64-pc-windows-gnu || true
    ;;
  *)
    echo "Error: unsupported platform: $(uname -s) $(uname -m)"
    echo "Supported: Linux x86_64/arm64, macOS Intel/Apple Silicon, Windows x64."
    exit 1
    ;;
esac

if [ "$download_ok" -ne 1 ]; then
  echo "Download failed: no native binary for this platform (or GitHub is unreachable)."
  echo "Manual options:"
  echo "  cargo install --git https://github.com/${REPO_SLUG} --locked"
  echo "  Open https://github.com/${REPO_SLUG}/releases/latest and download the tarball."
  exit 1
fi

finish
echo "Done."
