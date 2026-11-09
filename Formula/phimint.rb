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
  version "0.8.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.8.0/phimint-0.8.0-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.8.0/phimint-0.8.0-darwin-aarch64.tar.gz"
      sha256 "0c99851a88d42c35789038f9fbf52827523bf3b430088d6849e2e2093eb67229"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.8.0/phimint-0.8.0-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.8.0/phimint-0.8.0-darwin-x86_64.tar.gz"
      sha256 "2e87b09c23a3bd4601ed5659d1236a3e60a0a36549a655c7800bd7a9c1b62f9a"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.8.0/phimint-0.8.0-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.8.0/phimint-0.8.0-linux-aarch64.tar.gz"
      sha256 "474442d4b6343f1cf03d25674d2d1ecdaf72bf15a5e0b637e47ca3b767970cb9"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.8.0/phimint-0.8.0-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.8.0/phimint-0.8.0-linux-x86_64.tar.gz"
      sha256 "3542c8ade9a3357738595460136d76a84d1f0803ee183530074fa58534ddf4f8"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
