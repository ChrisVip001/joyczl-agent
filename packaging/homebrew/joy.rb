# The Homebrew formula for Joy.
#
# Built from source rather than installed as a binary: Joy is a cargo workspace, so any
# install path other than the release tarball has to compile anyway — and `cargo install`
# is the standard move in the cargo world.
#
# Usage:
#
#   brew tap <your tap> && brew install joy
#
# To check it locally:
#
#   brew install --build-from-source ./packaging/homebrew/joy.rb
#   brew test joy
#
# How to get the real checksum after tagging a release:
#
#   brew fetch --build-from-source ./packaging/homebrew/joy.rb
class Joy < Formula
  desc "Local-first personal assistant with long-term memory"
  homepage "https://github.com/ChrisVip001/joyczl-agent"
  url "https://github.com/ChrisVip001/joyczl-agent/archive/refs/tags/v0.17.0.tar.gz"
  # The sha256 is rewritten by `just release-hash <tag>` from that same
  # /archive/refs/tags/ URL (see packaging/RELEASING.md). Do not compute it from the
  # GitHub API's /tarball/ endpoint: that one is a legacy archive and the bytes differ.
  sha256 "f49e3d5e3eb0021bf54cdde731a3e5acbc0b32b25623f57f691c6f6967b780bd"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/ChrisVip001/joyczl-agent.git", branch: "main"

  depends_on "rust" => :build

  def install
    # The CLI crate is a workspace member: install from its directory, and dependency
    # resolution still goes through the root Cargo.toml.
    system "cargo", "install", *std_cargo_args(path: "joy-rs/joyczl-cli")
  end

  service do
    # What stays resident is the gateway/cockpit side, not the REPL. JOY_HOME stays in the
    # user's home directory.
    run [opt_bin/"joy", "dashboard"]
    keep_alive true
    environment_variables JOY_HOME: "${HOME}/.joy"
  end

  test do
    assert_match "joy #{version}", shell_output("#{bin}/joy --version")
    # An unknown subcommand prints usage and exits non-zero — that path needs no API key and
    # touches no network, which makes it a good post-install smoke test.
    assert_match "Joy", shell_output("#{bin}/joy definitely-not-a-command 2>&1", 1)
  end
end
