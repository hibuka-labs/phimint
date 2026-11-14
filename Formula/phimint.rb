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
  version "0.10.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.10.0/phimint-0.10.0-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.10.0/phimint-0.10.0-darwin-aarch64.tar.gz"
      sha256 "833e6cbf2f2b2e13c205156a74621ef57f247987bb6378c23b091cd7fd55ddd7"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.10.0/phimint-0.10.0-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.10.0/phimint-0.10.0-darwin-x86_64.tar.gz"
      sha256 "04bb06c29b859fb726b9eb919e19ddc0713492de9eece06ea90727d1b2244143"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.10.0/phimint-0.10.0-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.10.0/phimint-0.10.0-linux-aarch64.tar.gz"
      sha256 "73c85716f829c49cac951490fd325a417c91b2f7fb7cd27bd880289cc96c8b7a"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.10.0/phimint-0.10.0-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.10.0/phimint-0.10.0-linux-x86_64.tar.gz"
      sha256 "9283db2e7465e032937c6bacdf375b43adccb8b7ae73dedab93819aeb85ded64"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
