#!/bin/sh
# Signs and publishes a draft release made by the release workflow:
#   scripts/sign-release.sh v0.2.0
# Downloads the draft's SHA256SUMS, checks every archive against it, signs it
# with the maintainer's SSH key, verifies the signature against
# signing/allowed_signers, uploads SHA256SUMS.sig, and publishes the release.
set -eu

tag="${1:?usage: sign-release.sh vX.Y.Z}"
repo="tlpinney/remuda"
namespace="remuda-release"
signer="tlpinney"
key="${REMUDA_SIGNING_KEY:-$HOME/.ssh/id_ed25519}"

root="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

draft="$(gh release view "$tag" --repo "$repo" --json isDraft --jq .isDraft)"
[ "$draft" = true ] || { echo "$tag is not a draft release; refusing to change it" >&2; exit 1; }

gh release download "$tag" --repo "$repo" --dir "$tmp"
(cd "$tmp" && shasum -a 256 -c SHA256SUMS)

echo "Signing SHA256SUMS for $tag with $key"
ssh-keygen -Y sign -f "$key" -n "$namespace" "$tmp/SHA256SUMS"
ssh-keygen -Y verify -f "$root/signing/allowed_signers" -I "$signer" -n "$namespace" \
  -s "$tmp/SHA256SUMS.sig" < "$tmp/SHA256SUMS"

gh release upload "$tag" "$tmp/SHA256SUMS.sig" --repo "$repo"
gh release edit "$tag" --repo "$repo" --draft=false --latest
echo "Published $tag: https://github.com/$repo/releases/tag/$tag"
