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
  version "0.11.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.11.0/phimint-0.11.0-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.11.0/phimint-0.11.0-darwin-aarch64.tar.gz"
      sha256 "93d5f214250604ad891331f9a456ba59d4bd497713b50d4422c46e3bf4fbcd7c"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.11.0/phimint-0.11.0-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.11.0/phimint-0.11.0-darwin-x86_64.tar.gz"
      sha256 "da62fc8512306b8e379a2a4accefdc50b85830773fb083afa7d096e7a28227e8"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.11.0/phimint-0.11.0-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.11.0/phimint-0.11.0-linux-aarch64.tar.gz"
      sha256 "cb7f65954e460203469fe024f627bc20e501a8ac849f2b57ca4c4ee189f8186b"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.11.0/phimint-0.11.0-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.11.0/phimint-0.11.0-linux-x86_64.tar.gz"
      sha256 "ea57fbaac331543c3505bda11bcbb0b259c9d34ab9cca50c51c69479bb7828e2"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
