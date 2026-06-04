class Tunnix < Formula
  desc "Encrypted SOCKS5/HTTP proxy tunnel over HTTP/SSE"
  homepage "https://github.com/aeroxy/tunnix"
  version "0.3.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/aeroxy/tunnix/releases/download/0.3.0/tunnix_macos_arm64.zip"
      sha256 "fd02b6b89a610f38144b439579d94e72ef49658c7c671269fcf8ee62bc5de120"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/aeroxy/tunnix/releases/download/0.3.0/tunnix_linux_x86_64.zip"
      sha256 "f002822f5836649962d845915cb1486ce8a6815a3653a49383313ecbfe1a1487"
    end
  end

  def install
    bin.install "tunnix"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tunnix --version")
  end
end
