class Tunnix < Formula
  desc "Encrypted SOCKS5/HTTP proxy tunnel over HTTP/SSE"
  homepage "https://github.com/aeroxy/tunnix"
  version "0.4.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/aeroxy/tunnix/releases/download/0.4.0/tunnix_macos_arm64.zip"
      sha256 "c4f1f61d1687a1c3b2ee67f31090fb1e0409a22b7de065d200cbc2320fae46ab"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/aeroxy/tunnix/releases/download/0.4.0/tunnix_linux_x86_64.zip"
      sha256 "1afb23b7022b373db91d8120b39e5a5f17101d41535ad1da771951167e18705f"
    end
  end

  def install
    bin.install "tunnix"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tunnix --version")
  end
end
