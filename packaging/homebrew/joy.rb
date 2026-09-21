# Joy 的 Homebrew formula 模板。
#
# 从源码构建而不是装二进制：Joy 是一个 cargo workspace，发布 tarball 之外的
# 任何安装方式都要重新编译；`cargo install` 正好是 cargo 世界的标准动作。
#
# 用法（先把 url / sha256 换成真发布物）：
#
#   brew tap <你的 tap> && brew install joy
#
# 本地验证：
#
#   brew install --build-from-source ./packaging/homebrew/joy.rb
#   brew test joy
#
# 发布 tag 之后拿到真实校验和的办法：
#
#   brew fetch --build-from-source ./packaging/homebrew/joy.rb
class Joy < Formula
  desc "Local-first personal assistant with long-term memory"
  homepage "https://github.com/ChrisVip001/joyczl-agent"
  url "https://github.com/ChrisVip001/joyczl-agent/archive/refs/tags/v0.5.0.tar.gz"
  # 与 v0.5.0 这个 tag 的归档逐字节核对过（同一 URL 取两次哈希一致）。
  # 注意别拿 GitHub API 的 /tarball/ 端点去算：它那边是 legacy 归档，字节不同。
  # 发布新版本时重算：
  #   curl -L <url> | shasum -a 256
  sha256 "c775b8bc89396cc375b4590e63e226409c0fb54c2c2ed4b6bd52416faafa7d91"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/ChrisVip001/joyczl-agent.git", branch: "main"

  depends_on "rust" => :build

  def install
    # CLI crate 是 workspace 的成员：从它的目录装，依赖解析照样走根 Cargo.toml。
    system "cargo", "install", *std_cargo_args(path: "joy-rs/joyczl-cli")
  end

  service do
    # 常驻的是网关/驾驶舱那侧，不是 REPL。JOY_HOME 留在用户目录里。
    run [opt_bin/"joy", "dashboard"]
    keep_alive true
    environment_variables JOY_HOME: "${HOME}/.joy"
  end

  test do
    assert_match "joy #{version}", shell_output("#{bin}/joy --version")
    # 不认识的子命令要打印用法并以非零退出 —— 这条路径不需要 API key、
    # 不碰网络，适合当安装后的冒烟测试。
    assert_match "Joy", shell_output("#{bin}/joy definitely-not-a-command 2>&1", 1)
  end
end
