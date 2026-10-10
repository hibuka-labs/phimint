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
  version "0.3.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.3.0/phimint-0.3.0-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.3.0/phimint-0.3.0-darwin-aarch64.tar.gz"
      sha256 "5fff1eef5f038030fa053b1d8a4ca6ad859716f032eaa20c2b47bf671b70eb2e"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.3.0/phimint-0.3.0-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.3.0/phimint-0.3.0-darwin-x86_64.tar.gz"
      sha256 "b63185d88ed6db201cbfa7ed870de652a7a643aac0b8eb1dc6c566f2eb0820af"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.3.0/phimint-0.3.0-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.3.0/phimint-0.3.0-linux-aarch64.tar.gz"
      sha256 "fec5d31581064e919765b94c923e5555c504fbc73379f4c743072f87ea4e44d0"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.3.0/phimint-0.3.0-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.3.0/phimint-0.3.0-linux-x86_64.tar.gz"
      sha256 "49615e7dea3cc14eadc963031bf29881db849a21245013987911735d5fc347d3"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
