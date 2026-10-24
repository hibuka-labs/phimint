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
  version "0.6.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.6.0/phimint-0.6.0-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.6.0/phimint-0.6.0-darwin-aarch64.tar.gz"
      sha256 "075278b5b08b00b6c8fa0c4e91b463e9e497abc2756c6772cc6971982f000de6"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.6.0/phimint-0.6.0-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.6.0/phimint-0.6.0-darwin-x86_64.tar.gz"
      sha256 "34c082ade0942d513a19203cff2692e2cb0470b923e2c32401d4313750de137d"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.6.0/phimint-0.6.0-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.6.0/phimint-0.6.0-linux-aarch64.tar.gz"
      sha256 "eb21e90f2db3b5df7757f403a57e4347d830d04133ba0c10a3544c9c0aea4771"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.6.0/phimint-0.6.0-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.6.0/phimint-0.6.0-linux-x86_64.tar.gz"
      sha256 "ced7a52066bba46d933a96f7becb5d16c5bb030ead5c0de9598b35217f4fd912"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
