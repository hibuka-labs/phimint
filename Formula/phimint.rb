# Homebrew formula for phimint.
#
#   brew install hibuka-labs/phimint/phimint
#
# Source of truth: hibuka-labs/phimint (Formula/phimint.rb). Synced to the tap
# repository hibuka-labs/homebrew-phimint by deploy/release.sh at every release.
# The version, URLs and sha256 values below are rewritten at release — do not
# hand-edit them.
class Phimint < Formula
  desc "Terminal AI coding agent built on phi-agent"
  homepage "https://github.com/hibuka-labs/phimint"
  version "0.5.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.5.0/phimint-0.5.0-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.5.0/phimint-0.5.0-darwin-aarch64.tar.gz"
      sha256 "5409ee2b41c8f2a46869399edcb3e9e13c5a3e390960a615b689af06d8032dc0"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.5.0/phimint-0.5.0-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.5.0/phimint-0.5.0-darwin-x86_64.tar.gz"
      sha256 "ea15ed94bd16f58a392dcec709c940b4beff8d47c61a610478d5a1498bff55fe"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.5.0/phimint-0.5.0-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.5.0/phimint-0.5.0-linux-aarch64.tar.gz"
      sha256 "8a0de7aaaab8eb74cf7081fd1b7b28d87e1d1e7767dfeb97fd4eb48f0b58fcae"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.5.0/phimint-0.5.0-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.5.0/phimint-0.5.0-linux-x86_64.tar.gz"
      sha256 "832174e0943fd2618e16c9d170afa400ebb32df5b399be15871c432750ad94a2"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
