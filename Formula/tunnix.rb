class Tunnix < Formula
  desc "Encrypted SOCKS5/HTTP proxy tunnel over HTTP/SSE"
  homepage "https://github.com/aeroxy/tunnix"
  version "0.2.2"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/aeroxy/tunnix/releases/download/0.2.2/tunnix_macos_arm64.zip"
      sha256 "6fa27a5f734eb03e4ed1daa702ec0c25a5f629877e370487479ea069b1a6b063"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/aeroxy/tunnix/releases/download/0.2.2/tunnix_linux_x86_64.zip"
      sha256 "bd997605f911c37406e851a026f524fcb302b9d87f6faf6391cb2aa95347342f"
    end
  end

  def install
    bin.install "tunnix"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tunnix --version")
  end
end
