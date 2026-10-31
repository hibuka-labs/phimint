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
  version "0.7.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.7.1/phimint-0.7.1-darwin-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.7.1/phimint-0.7.1-darwin-aarch64.tar.gz"
      sha256 "218955e30d4fb255ab6948c57cf8cca580b3be3eb15e37815656e6613bb5d389"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.7.1/phimint-0.7.1-darwin-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.7.1/phimint-0.7.1-darwin-x86_64.tar.gz"
      sha256 "9f4908f00c5dd5d18a17445a32d952a89fb2c551a91b0c236883dc53df2fec04"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.7.1/phimint-0.7.1-linux-aarch64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.7.1/phimint-0.7.1-linux-aarch64.tar.gz"
      sha256 "b0063bf08bbd68a8613fb246d526b2d646dd454848664890790f322bc8f92878"
    end
    on_intel do
      url "https://github.com/hibuka-labs/phimint/releases/download/v0.7.1/phimint-0.7.1-linux-x86_64.tar.gz"
      mirror "https://gitee.com/chenkangzeng_admin/phimint/releases/download/v0.7.1/phimint-0.7.1-linux-x86_64.tar.gz"
      sha256 "56451c547285b714b55a05d9f943dd06eaa459f9762aebc0893babbde32f2351"
    end
  end

  def install
    bin.install "phimint"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/phimint --version")
  end
end
