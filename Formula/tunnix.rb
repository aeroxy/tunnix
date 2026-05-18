class Tunnix < Formula
  desc "Encrypted SOCKS5/HTTP proxy tunnel over HTTP/SSE"
  homepage "https://github.com/aeroxy/tunnix"
  version "0.2.3"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/aeroxy/tunnix/releases/download/0.2.3/tunnix_macos_arm64.zip"
      sha256 "74d355fb8aeb0856d0df5054237df8b3af1957e3efa1c9e2ecb23ab95afbfaf2"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/aeroxy/tunnix/releases/download/0.2.3/tunnix_linux_x86_64.zip"
      sha256 "15f03fd737c5ffd5d16ed98adda0061d86ca313d4b3abb7d001cfa569a24daa8"
    end
  end

  def install
    bin.install "tunnix"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tunnix --version")
  end
end
