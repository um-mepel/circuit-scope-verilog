#!/usr/bin/env bash
# Post-release SHA bumper for the Homebrew cask + formula.
#
# Run AFTER:
#   1. `git push origin main`
#   2. `git tag vX.Y.Z && git push origin vX.Y.Z`
#   3. The release workflow finishes (~10-15 min) and the draft release
#      is published from the GitHub UI.
#
# It downloads the three release assets (ARM DMG, Intel DMG, source tarball),
# computes their SHA256 sums, and patches `Casks/circuit-scope.rb` +
# `Formula/csverilog.rb` in place. Then prints the next steps for pushing
# the bump to the Homebrew tap repo.
#
# Usage:
#   homebrew/bump-shas.sh            # picks the version from Casks/circuit-scope.rb
#   homebrew/bump-shas.sh 0.3.0      # explicit version override
set -euo pipefail

repo="um-mepel/circuit-scope-verilog"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cask="$here/Casks/circuit-scope.rb"
formula="$here/Formula/csverilog.rb"

# Resolve version.
if [ "${1:-}" != "" ]; then
  version="$1"
else
  version="$(grep -E 'version "[0-9]+\.[0-9]+\.[0-9]+"' "$cask" | head -1 | sed -E 's/.*"([0-9]+\.[0-9]+\.[0-9]+)".*/\1/')"
fi
if [ -z "${version:-}" ]; then
  echo "error: could not determine version. Pass it as the first argument." >&2
  exit 1
fi
echo "Bumping SHAs for v$version"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

base="https://github.com/$repo/releases/download/v$version"

# DMG asset names use a dot between the words (GitHub asset normalization).
arm_dmg="Circuit.Scope_${version}_aarch64.dmg"
int_dmg="Circuit.Scope_${version}_x64.dmg"
src_tar="v${version}.tar.gz"

echo "Downloading release assets..."
curl -fLso "$tmp/$arm_dmg" "$base/$arm_dmg"
curl -fLso "$tmp/$int_dmg" "$base/$int_dmg"
curl -fLso "$tmp/$src_tar" "https://github.com/$repo/archive/refs/tags/v${version}.tar.gz"

arm_sha="$(shasum -a 256 "$tmp/$arm_dmg" | awk '{print $1}')"
int_sha="$(shasum -a 256 "$tmp/$int_dmg" | awk '{print $1}')"
src_sha="$(shasum -a 256 "$tmp/$src_tar" | awk '{print $1}')"

echo "  ARM DMG  sha256: $arm_sha"
echo "  Intel    sha256: $int_sha"
echo "  source   sha256: $src_sha"

# Patch the cask: first placeholder is the ARM SHA (on_arm block comes first),
# second is Intel. We use awk to replace only the first two TODO-tagged sha256
# placeholder lines, in order.
awk -v arm="$arm_sha" -v intel="$int_sha" '
  /TODO bump-shas\.sh/ && !seen_arm {
    sub(/sha256 "[0-9a-f]+"/, "sha256 \"" arm "\"")
    sub(/  # TODO bump-shas\.sh/, "")
    seen_arm = 1
    print
    next
  }
  /TODO bump-shas\.sh/ && seen_arm && !seen_intel {
    sub(/sha256 "[0-9a-f]+"/, "sha256 \"" intel "\"")
    sub(/  # TODO bump-shas\.sh/, "")
    seen_intel = 1
    print
    next
  }
  { print }
' "$cask" > "$cask.new" && mv "$cask.new" "$cask"

# Patch the formula: one placeholder for the source tarball.
awk -v src="$src_sha" '
  /TODO bump-shas\.sh/ && !seen {
    sub(/sha256 "[0-9a-f]+"/, "sha256 \"" src "\"")
    sub(/  # TODO bump-shas\.sh/, "")
    seen = 1
    print
    next
  }
  { print }
' "$formula" > "$formula.new" && mv "$formula.new" "$formula"

echo
echo "Patched:"
echo "  $cask"
echo "  $formula"
echo
echo "Next steps:"
echo "  1. Review the diff:   git -C \"$(dirname "$here")\" diff homebrew/"
echo "  2. Audit locally:     brew audit --cask  --new-cask    $cask"
echo "                         brew audit --formula --new-formula $formula"
echo "  3. Commit + push the bump to the tap repo:"
echo "       git clone https://github.com/um-mepel/homebrew-circuit-scope.git tap"
echo "       cp homebrew/Casks/circuit-scope.rb tap/Casks/"
echo "       cp homebrew/Formula/csverilog.rb  tap/Formula/"
echo "       cd tap && git add Casks Formula"
echo "       git commit -m \"Bump Circuit Scope + csverilog to v$version\""
echo "       git push"
