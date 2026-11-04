#!/usr/bin/env bash
# Refresh the Homebrew formula's version + sha256 from a git tag.
#
# Usage:  ./packaging/release-hash.sh v0.8.0
#
# Computes the archive checksum the same way Homebrew will fetch it
# (the /archive/refs/tags/ URL — NOT the legacy /tarball/ endpoint, whose
# bytes differ), then rewrites url/sha256 in packaging/homebrew/joy.rb.
set -euo pipefail

tag="${1:?用法: release-hash.sh v0.8.0}"
tag="${tag#v}"
repo="ChrisVip001/joyczl-agent"
url="https://github.com/${repo}/archive/refs/tags/v${tag}.tar.gz"
formula="$(dirname "$0")/homebrew/joy.rb"

echo "计算 ${url} 的 sha256 …"
sum="$(curl -fsSL "$url" | shasum -a 256 | awk '{print $1}')"

sed -i.bak \
    -e "s|archive/refs/tags/v[0-9][0-9.]*\.tar\.gz|archive/refs/tags/v${tag}.tar.gz|" \
    -e "s|^  sha256 \".*\"|  sha256 \"${sum}\"|" \
    "$formula"
rm -f "${formula}.bak"

echo "已更新 ${formula}:"
grep -E "url |sha256" "$formula"
