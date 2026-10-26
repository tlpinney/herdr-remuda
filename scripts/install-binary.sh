#!/bin/sh
# Herdr runs this when installing or updating the plugin. It downloads the
# prebuilt binary for this platform from the GitHub Release matching the
# version in Cargo.toml and installs it only after verifying:
#   1. SHA256SUMS carries a valid SSH signature from a key in
#      signing/allowed_signers (made offline by the maintainer, not by CI);
#   2. the downloaded archive matches its line in SHA256SUMS.
# Without a release for this version, or without ssh-keygen to verify it,
# it builds from source instead. A failed verification stops the install.
set -eu

repo="tlpinney/remuda"
namespace="remuda-release"
signer="tlpinney"
dest="target/release/remuda"

build_from_source() {
  echo "remuda: $1; building from source with cargo" >&2
  if ! command -v cargo >/dev/null 2>&1; then
    echo "remuda: cargo not found; install Rust from https://rustup.rs" >&2
    exit 1
  fi
  exec cargo build --release --locked
}

fail() {
  echo "remuda: $1; refusing to install the download" >&2
  exit 1
}

case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux) os=linux ;;
  *) build_from_source "no prebuilt binary for $(uname -s)" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) build_from_source "no prebuilt binary for $(uname -m)" ;;
esac
command -v ssh-keygen >/dev/null 2>&1 || build_from_source "ssh-keygen not found to verify the release signature"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$version" ] || build_from_source "cannot read the version from Cargo.toml"

asset="remuda-$os-$arch.tar.gz"
base="https://github.com/$repo/releases/download/v$version"

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 --retry 2 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q --https-only -O "$2" "$1"
  else
    return 1
  fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    return 1
  fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

download "$base/SHA256SUMS" "$tmp/SHA256SUMS" || build_from_source "no release for v$version"
download "$base/SHA256SUMS.sig" "$tmp/SHA256SUMS.sig" || fail "release v$version has no signature"
download "$base/$asset" "$tmp/$asset" || fail "release v$version has no $asset"

ssh-keygen -Y verify -f signing/allowed_signers -I "$signer" -n "$namespace" \
  -s "$tmp/SHA256SUMS.sig" < "$tmp/SHA256SUMS" >/dev/null 2>&1 \
  || fail "SHA256SUMS signature does not verify against signing/allowed_signers"

expected="$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || fail "$asset is not listed in the signed SHA256SUMS"
actual="$(sha256 "$tmp/$asset")" || build_from_source "no sha256 tool to verify the download"
[ "$expected" = "$actual" ] || fail "checksum mismatch for $asset (expected $expected, got $actual)"

# Extract only the binary, never other archive members.
tar -xzf "$tmp/$asset" -C "$tmp" remuda
mkdir -p "$(dirname "$dest")"
mv "$tmp/remuda" "$dest"
chmod 755 "$dest"
echo "remuda: installed signed prebuilt v$version for $os-$arch"
