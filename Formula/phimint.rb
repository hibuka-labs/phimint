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
  version "0.13.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.13.0/phimint-0.13.0-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.13.0/phimint-0.13.0-darwin-aarch64.tar.gz"
      sha256 "0fe18e070d11b88285d59cce3b94dc90baec6cd12e94e77c226363c6262a7e22"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.13.0/phimint-0.13.0-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.13.0/phimint-0.13.0-darwin-x86_64.tar.gz"
      sha256 "fd4d8ec1f9bc64d4f10dbd1aee210c977cdd2bd27d8b485096579309e5e208a3"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.13.0/phimint-0.13.0-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.13.0/phimint-0.13.0-linux-aarch64.tar.gz"
      sha256 "58fd6da717b83892e9b1737db379d6b66ff5fe9947752de8fb78e5153505f2e6"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.13.0/phimint-0.13.0-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.13.0/phimint-0.13.0-linux-x86_64.tar.gz"
      sha256 "1bec79036a9d06fef0716092a9c05265a3636f603ee7cb0ec6e649fc1ca848a3"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
