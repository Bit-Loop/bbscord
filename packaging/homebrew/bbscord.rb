class Bbscord < Formula
  desc "Retro BBS-styled Rust Discord TUI client with DAVE E2EE voice, WebRTC video, GIFs & btop themes"
  homepage "https://github.com/Bit-Loop/bbscord"
  version "1.0.0"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/Bit-Loop/bbscord/releases/download/v1.0.0/bbscord-1.0.0-macos-arm64.tar.gz"
    end
    on_intel do
      url "https://github.com/Bit-Loop/bbscord/releases/download/v1.0.0/bbscord-1.0.0-macos-x86_64.tar.gz"
    end
  end

  depends_on "gstreamer"
  depends_on "opus"

  def install
    bin.install "bbscord"
  end

  test do
    assert_match "headless-smoke PASS", shell_output("#{bin}/bbscord --headless-smoke")
  end
end
