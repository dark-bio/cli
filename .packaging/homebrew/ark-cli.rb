# Release template filled with the version and downloaded artifact digests.
class @CLASS@ < Formula
  desc "Command line interface to Ark enclaves"
  homepage "https://dark.bio"
  version "@VERSION@"
  license "BSD-3-Clause"

  depends_on :macos
  conflicts_with "@CONFLICT@", because: "both install the ark command"

  on_arm do
    url "https://github.com/dark-bio/cli/releases/download/v@VERSION@/ark-@VERSION@-macos-arm64", using: :nounzip
    sha256 "@ARM64_SHA256@"
  end

  on_intel do
    url "https://github.com/dark-bio/cli/releases/download/v@VERSION@/ark-@VERSION@-macos-amd64", using: :nounzip
    sha256 "@AMD64_SHA256@"
  end

  resource "licenses" do
    url "https://github.com/dark-bio/cli/releases/download/v@VERSION@/LICENSES.txt", using: :nounzip
    sha256 "@LICENSES_SHA256@"
  end

  # Keep the command name stable and retain dependency notices beside the package.
  def install
    bin.install Dir["ark-#{version}-macos-*"].fetch(0) => "ark"
    chmod 0755, bin/"ark"
    generate_completions_from_executable(bin/"ark", "completions")
    resource("licenses").stage { pkgshare.install "LICENSES.txt" }
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/ark --version")
  end
end
