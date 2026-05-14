#!/usr/bin/env sh
set -eu

repo="btakita/tsift"
binary="tsift"
install_dir="${TSIFT_INSTALL_DIR:-$HOME/.local/bin}"
version="${TSIFT_VERSION:-latest}"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "tsift installer: missing required command: $1" >&2
    exit 1
  fi
}

download() {
  url="$1"
  out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$out" "$url"
  else
    echo "tsift installer: missing curl or wget" >&2
    exit 1
  fi
}

checksum() {
  file="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  else
    echo "tsift installer: missing shasum or sha256sum" >&2
    exit 1
  fi
}

os="$(uname -s)"
arch="$(uname -m)"

case "$os:$arch" in
  Linux:x86_64) target="x86_64-unknown-linux-gnu" ;;
  Darwin:x86_64) target="x86_64-apple-darwin" ;;
  Darwin:arm64 | Darwin:aarch64) target="aarch64-apple-darwin" ;;
  *)
    echo "tsift installer: unsupported platform: $os $arch" >&2
    exit 1
    ;;
esac

need tar
need awk
need mktemp
need mkdir
need chmod

asset="${binary}-${target}.tar.gz"
if [ "$version" = "latest" ]; then
  base_url="https://github.com/${repo}/releases/latest/download"
else
  case "$version" in
    v*) tag="$version" ;;
    *) tag="v$version" ;;
  esac
  base_url="https://github.com/${repo}/releases/download/${tag}"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

archive="$tmp/$asset"
sum_file="$tmp/$asset.sha256"

download "$base_url/$asset" "$archive"
download "$base_url/$asset.sha256" "$sum_file"

expected="$(awk '{print $1; exit}' "$sum_file")"
actual="$(checksum "$archive")"
if [ "$expected" != "$actual" ]; then
  echo "tsift installer: checksum mismatch for $asset" >&2
  echo "expected: $expected" >&2
  echo "actual:   $actual" >&2
  exit 1
fi

tar -xzf "$archive" -C "$tmp"
mkdir -p "$install_dir"
cp "$tmp/${binary}-${target}/$binary" "$install_dir/$binary"
chmod 755 "$install_dir/$binary"

echo "installed $binary to $install_dir/$binary"
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) echo "add $install_dir to PATH to run $binary directly" ;;
esac
