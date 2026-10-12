# Homebrew formula: prebuilt release binaries (macOS: Metal; Linux: static
# musl, CPU). This repo is its own tap:
#
#   brew tap axxapy/mademind https://github.com/axxapy/mademind
#   brew install axxapy/mademind/mademind
#   brew services start mademind      # serves ~/.config/mademind/config.toml
#
# New release: scripts/bump-formula.sh <version> (version + sha256 values
# from the release's SHA256SUMS).
class Mademind < Formula
  desc "Your notes, searchable by your AI agents (qmd-compatible search server)"
  homepage "https://github.com/axxapy/mademind"
  license "AGPL-3.0-only"

  on_macos do
    on_arm do
      url "https://github.com/axxapy/mademind/releases/download/v0.2.1/mademind-0.2.1-macos-arm64.tar.gz"
      sha256 "ab3a43c573fb588a4e20db8c4cda7d26c41d1b605317d4c9136f51ece9d3e600"
    end
    on_intel do
      url "https://github.com/axxapy/mademind/releases/download/v0.2.1/mademind-0.2.1-macos-x86_64.tar.gz"
      sha256 "ec9db394b7c2c3ca581f5fbc5d5dff3c2b6a1cdb15c3a9023e91df609202c272"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/axxapy/mademind/releases/download/v0.2.1/mademind-0.2.1-linux-arm64-musl.tar.gz"
      sha256 "fdfc4a9315a51275f1b262a0dc968969374453e2b641490567b41a17a078b42c"
    end
    on_intel do
      url "https://github.com/axxapy/mademind/releases/download/v0.2.1/mademind-0.2.1-linux-x86_64-musl.tar.gz"
      sha256 "8ab5fbbbbe472b23e0cdd9df5f5d5c4c57d391a2677aadc6751f23324372f4fc"
    end
  end

  # --HEAD builds from source (llama.cpp: cmake + a C++ toolchain).
  head do
    url "https://github.com/axxapy/mademind.git", branch: "master"
    depends_on "cmake" => :build
    depends_on "rust" => :build
  end

  def install
    if build.head?
      system "cargo", "install", *std_cargo_args
      pkgshare.install "clients", "config.example.toml", "config.reference.toml"
      return
    end

    bin.install "mademind"
    # Clients (MCP stdio proxy, pi extension, auth CLI) at a path that
    # survives upgrades: #{HOMEBREW_PREFIX}/opt/mademind/share/mademind/clients
    pkgshare.install "clients", "config.example.toml", "config.reference.toml"
  end

  def caveats
    <<~EOS
      The service reads ~/.config/mademind/config.toml; start from
        #{opt_pkgshare}/config.example.toml
      Index and models go to ~/.cache/mademind (models: ~2 GB on first use).
      Clients (MCP stdio proxy, pi extension): #{opt_pkgshare}/clients
    EOS
  end

  service do
    run [opt_bin/"mademind", "serve", "-c", "#{Dir.home}/.config/mademind/config.toml"]
    keep_alive true
    log_path var/"log/mademind.log"
    error_log_path var/"log/mademind.log"
  end

  test do
    assert_match "mademind #{version}", shell_output("#{bin}/mademind --version")
  end
end
