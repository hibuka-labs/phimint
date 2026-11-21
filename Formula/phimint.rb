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
  version "0.12.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.12.1/phimint-0.12.1-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.12.1/phimint-0.12.1-darwin-aarch64.tar.gz"
      sha256 "7d27ab9ba6c4e2ca7d1523c831f675f08b2b31c1e7406ddf96b8cac91f0a7d42"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.12.1/phimint-0.12.1-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.12.1/phimint-0.12.1-darwin-x86_64.tar.gz"
      sha256 "eaa0856dc50e73d920c93c98f526d0ad53b6b821bdba8988afe7749aa6e1ad41"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.12.1/phimint-0.12.1-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.12.1/phimint-0.12.1-linux-aarch64.tar.gz"
      sha256 "55b96f647b342880623edaa4a84ce6c65426760fbbeff41568674e90e373aa07"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.12.1/phimint-0.12.1-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.12.1/phimint-0.12.1-linux-x86_64.tar.gz"
      sha256 "28724639283f247b349e4463e1d4738ca27ce6d32d3bcb434f0fd638c2393624"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
