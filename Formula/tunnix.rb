class Tunnix < Formula
  desc "Encrypted SOCKS5/HTTP proxy tunnel over HTTP/SSE"
  homepage "https://github.com/aeroxy/tunnix"
  version "0.2.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/aeroxy/tunnix/releases/download/0.2.0/tunnix_macos_arm64.zip"
      sha256 "cce8c790098993259fef9c23e93d6ecfa51ffa7a05f6b219464bbf0d2d7c9fa3"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/aeroxy/tunnix/releases/download/0.2.0/tunnix_linux_x86_64.zip"
      sha256 "407f98f98975bf902582b1c2cf022eab045d045a1502f67a3db3b8982dae168e"
    end
  end

  def install
    bin.install "tunnix"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tunnix --version")
  end
end
