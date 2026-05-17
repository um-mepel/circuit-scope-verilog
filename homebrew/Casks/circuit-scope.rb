cask "circuit-scope" do
  version "0.2.2"

  # SHA-256 checksums for the DMG attached to the GitHub Release.
  # Regenerate after each release with:
  #   shasum -a 256 "Circuit.Scope_<version>_aarch64.dmg"
  #   shasum -a 256 "Circuit.Scope_<version>_x64.dmg"
  # GitHub Releases normalises the space in Tauri's DMG product name to a dot,
  # so the asset is served at `Circuit.Scope_<ver>_<arch>.dmg`.
  on_arm do
    sha256 "3d0155d0b193c662dfa47bdb503623ac88fb9add7f601ebcac66e36f49022518"
    url "https://github.com/um-mepel/circuit-scope-verilog/releases/download/v#{version}/Circuit.Scope_#{version}_aarch64.dmg"
  end
  on_intel do
    sha256 "5c82c7a7b0241544c89cacabb7e6ebff65557649f865be978cda66c675dc535e"
    url "https://github.com/um-mepel/circuit-scope-verilog/releases/download/v#{version}/Circuit.Scope_#{version}_x64.dmg"
  end

  name "Circuit Scope"
  desc "Verilog (IEEE 1364) IDE: edit, simulate to VCD, waveform viewer"
  homepage "https://github.com/um-mepel/circuit-scope-verilog"

  livecheck do
    url :url
    strategy :github_latest
  end

  auto_updates false
  depends_on macos: ">= :big_sur"

  app "Circuit Scope.app"

  zap trash: [
    "~/Library/Application Support/com.circuitscope.app",
    "~/Library/Caches/com.circuitscope.app",
    "~/Library/Logs/com.circuitscope.app",
    "~/Library/Preferences/com.circuitscope.app.plist",
    "~/Library/Saved Application State/com.circuitscope.app.savedState",
    "~/Library/WebKit/com.circuitscope.app",
  ]
end
