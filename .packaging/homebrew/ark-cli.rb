class ArkCli < Formula
  desc "Command line for Dark Bio Arks"
  homepage "https://dark.bio"
  version "@VERSION@"
  license "BSD-3-Clause"

  depends_on :macos

  on_arm do
    url "https://github.com/dark-bio/cli/releases/download/v#{version}/ark-#{version}-macos-arm64", using: :nounzip
    sha256 "@ARM64_SHA256@"
  end

  on_intel do
    url "https://github.com/dark-bio/cli/releases/download/v#{version}/ark-#{version}-macos-amd64", using: :nounzip
    sha256 "@AMD64_SHA256@"
  end

  resource "licenses" do
    url "https://github.com/dark-bio/cli/releases/download/v@VERSION@/LICENSES.txt", using: :nounzip
    sha256 "@LICENSES_SHA256@"
  end

  def install
    bin.install Dir["ark-#{version}-macos-*"].fetch(0) => "ark"
    chmod 0755, bin/"ark"
    resource("licenses").stage { pkgshare.install "LICENSES.txt" }
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/ark --version")
  end
end
