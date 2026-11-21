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
  version "0.12.2"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.12.2/phimint-0.12.2-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.12.2/phimint-0.12.2-darwin-aarch64.tar.gz"
      sha256 "ed4d4e382d0ea798f11cf0cb53e009c97a57105ec4d26a0bd79fd550b6909588"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.12.2/phimint-0.12.2-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.12.2/phimint-0.12.2-darwin-x86_64.tar.gz"
      sha256 "3f38dfcadfebfd98314242ea82d1b1046e811d8eac5face9be79eb8fbccd6569"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.12.2/phimint-0.12.2-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.12.2/phimint-0.12.2-linux-aarch64.tar.gz"
      sha256 "4c26a8cbbb293cfeda7909d2bf600acbf00b90263874b5f36a975c89dc4823ae"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.12.2/phimint-0.12.2-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.12.2/phimint-0.12.2-linux-x86_64.tar.gz"
      sha256 "99c8bc7b9e2d00384817da29c1c061bc4a44eb9f1f9626c7c63d6a5a829edae9"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
