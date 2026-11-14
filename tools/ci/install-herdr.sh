#!/bin/bash
# Installs one Herdr release at ~/.local/bin/herdr, after checking it against
# the sha256 GitHub publishes for it. That path is where Herdr's own installer
# puts it, and verify.py's process captures name the binary by it.
#
#   tools/ci/install-herdr.sh v0.9.5
#
# For GitHub's Macs only: it replaces whatever Herdr is at that path. Needs gh
# with GH_TOKEN set.
set -euo pipefail

[ "${GITHUB_ACTIONS:-}" = true ] || { echo "install-herdr.sh is for CI: it replaces ~/.local/bin/herdr" >&2; exit 1; }
tag=${1:?usage: install-herdr.sh <Herdr release tag>}
[[ $tag =~ ^(v[0-9]+\.[0-9]+\.[0-9]+|preview-[0-9]{4}-[0-9]{2}-[0-9]{2}-[0-9a-f]+)$ ]] ||
  { echo "not a Herdr release tag: $tag" >&2; exit 1; }
case "$(uname -m)" in
  x86_64) asset=herdr-macos-x86_64 ;;
  arm64) asset=herdr-macos-aarch64 ;;
  *) echo "Herdr has no macOS build for $(uname -m)" >&2; exit 1 ;;
esac

want=$(gh api "repos/herdrdev/herdr/releases/tags/$tag" --jq ".assets[] | select(.name == \"$asset\") | .digest")
[[ $want =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "GitHub gives no sha256 for $asset in $tag" >&2; exit 1; }
download=$(mktemp)
curl -fsSL --retry 3 -o "$download" "https://github.com/herdrdev/herdr/releases/download/$tag/$asset"
got="sha256:$(shasum -a 256 "$download" | cut -d' ' -f1)"
[ "$got" = "$want" ] || { echo "$asset in $tag: got $got, GitHub says $want" >&2; exit 1; }

mkdir -p ~/.local/bin
chmod +x "$download"
mv "$download" ~/.local/bin/herdr
~/.local/bin/herdr --version
