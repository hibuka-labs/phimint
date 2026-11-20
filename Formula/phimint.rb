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
  version "0.12.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.12.0/phimint-0.12.0-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.12.0/phimint-0.12.0-darwin-aarch64.tar.gz"
      sha256 "427a68d8191da0b098aec2355b3908bfc7f011194cf9cc2626a99b7b607339dc"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.12.0/phimint-0.12.0-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.12.0/phimint-0.12.0-darwin-x86_64.tar.gz"
      sha256 "07d029cc3e8fcf8a6d223eeaab4d4587f0211cb4f74f9c3b15b6aaa24c12a194"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.12.0/phimint-0.12.0-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.12.0/phimint-0.12.0-linux-aarch64.tar.gz"
      sha256 "50bbd124f032acaa09ff467b7627640d009ce05e97b1ecea1b830436052c8621"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.12.0/phimint-0.12.0-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.12.0/phimint-0.12.0-linux-x86_64.tar.gz"
      sha256 "17a39e0028e3b56bffcd807bf8a43ffcad54dff4e1fff849f18a14ca53862c26"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
