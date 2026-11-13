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
  version "0.9.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.9.0/phimint-0.9.0-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.9.0/phimint-0.9.0-darwin-aarch64.tar.gz"
      sha256 "f0cca228a948be4a566e95e93bf2a42d34254c74d34662cfdb417399f9340e2d"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.9.0/phimint-0.9.0-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.9.0/phimint-0.9.0-darwin-x86_64.tar.gz"
      sha256 "2624d1a513b8ebc38efe6b057c8ac11639323f0bf01f1c517977662b58253de2"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.9.0/phimint-0.9.0-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.9.0/phimint-0.9.0-linux-aarch64.tar.gz"
      sha256 "acf7b9d65b612da59d51b50c719519ac74b601d8028336ec5b40bb202e07ad6e"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.9.0/phimint-0.9.0-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.9.0/phimint-0.9.0-linux-x86_64.tar.gz"
      sha256 "832b3ec95c4187b80b89aa74223283858d8ee5978040b48fbd14c2bfd5f132cf"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
