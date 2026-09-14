class Tunnix < Formula
  desc "Encrypted SOCKS5/HTTP proxy tunnel over HTTP/SSE"
  homepage "https://github.com/aeroxy/tunnix"
  version "0.5.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/aeroxy/tunnix/releases/download/0.5.0/tunnix_macos_arm64.zip"
      sha256 "0ae8413600a7e086b941417fc58104f3ae870cee595fa5e23c94ec99fb48cc3f"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/aeroxy/tunnix/releases/download/0.5.0/tunnix_linux_x86_64.zip"
      sha256 "f9d266d227e1c7a9f9b8bd3d6a3eeedba9d09e7282f8c1a7ca93a4887c09972a"
    end
  end

  def install
    bin.install "tunnix"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tunnix --version")
  end
end
