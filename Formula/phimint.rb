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
  version "0.2.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.2.0/phimint-0.2.0-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.2.0/phimint-0.2.0-darwin-aarch64.tar.gz"
      sha256 "45afb960c2fba75dc959d46c50aa3e093894a477261a99a025dc257af85b65de"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.2.0/phimint-0.2.0-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.2.0/phimint-0.2.0-darwin-x86_64.tar.gz"
      sha256 "4cb90d35a163451f836d743c95188aff6a4f599d33f61731b1e52b67921c750d"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.2.0/phimint-0.2.0-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.2.0/phimint-0.2.0-linux-aarch64.tar.gz"
      sha256 "90fcb05798049d259b856f6402ce2fc29118396943111b9e7e617551661bd49f"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.2.0/phimint-0.2.0-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.2.0/phimint-0.2.0-linux-x86_64.tar.gz"
      sha256 "e5a37b2197d75f3e1340d1cc2320f1eb7d16666a89aeac296a6c4093e7d17cd7"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
