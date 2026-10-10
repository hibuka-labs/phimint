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
  version "0.4.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.4.0/phimint-0.4.0-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.4.0/phimint-0.4.0-darwin-aarch64.tar.gz"
      sha256 "7a5765b51afa13e4b151283097d72f334a789b4674458eb9eae2c1b3ba24a4bc"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.4.0/phimint-0.4.0-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.4.0/phimint-0.4.0-darwin-x86_64.tar.gz"
      sha256 "ead3acf7cba88640c89e2af022bc69f0b5cd47979d3f7f04eaaeaee7cd54418c"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.4.0/phimint-0.4.0-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.4.0/phimint-0.4.0-linux-aarch64.tar.gz"
      sha256 "a82297f85db20f5f052fcb207ef9e69dcc1d3e76990cef86111675ce99702e71"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.4.0/phimint-0.4.0-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.4.0/phimint-0.4.0-linux-x86_64.tar.gz"
      sha256 "ad909a934ddfb25de1b258ad6d0331e711340aac8147675bb417d00f313ba54c"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
