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
  version "0.15.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.15.0/phimint-0.15.0-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.15.0/phimint-0.15.0-darwin-aarch64.tar.gz"
      sha256 "0a0400e48f702390f370a05316ab6d7727d0390d829f514855efff715061c84e"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.15.0/phimint-0.15.0-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.15.0/phimint-0.15.0-darwin-x86_64.tar.gz"
      sha256 "2a497f152fa34a7c57d91c65e2b367d8d968c72a7c72b4abcfb8db05fea179f4"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.15.0/phimint-0.15.0-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.15.0/phimint-0.15.0-linux-aarch64.tar.gz"
      sha256 "e674a57e724b2d8d473fd43f985971a7fe6b9cfd52c9015b6bfbd3a097f6ff1d"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.15.0/phimint-0.15.0-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.15.0/phimint-0.15.0-linux-x86_64.tar.gz"
      sha256 "621617fc63bf7d522472ce295635a1a8a4458274715bf416595d9309056b2356"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
