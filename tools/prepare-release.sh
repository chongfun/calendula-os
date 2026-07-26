#!/usr/bin/env bash
# Prepare a CalendulaOS release: sync every hand-maintained version/size label
# to the given semver, and verify the firmware carries the matching stamps.
#
# Usage: tools/prepare-release.sh <version>     e.g. tools/prepare-release.sh 0.5.0
#
# What it updates:
#   fw/Cargo.toml    package version — the app descriptor's `version` stamp is
#                    env!("CARGO_PKG_VERSION"), so the tag lies unless this matches.
#   Cargo.lock       refreshed by the build below.
#   web/index.html   the flasher's Version cell, Size cell (measured from the
#                    freshly built image), and the "vX.Y.Z release notes" line.
#
# What it deliberately does NOT do: commit, push, tag, or touch GitHub.
# The release itself is created by CI when a v* tag is pushed — never
# pre-create it by hand (see docs/agents/release.md).
set -euo pipefail
cd "$(dirname "$0")/.."

VER="${1:?usage: tools/prepare-release.sh <version, e.g. 0.5.0>}"
[[ "$VER" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "error: '$VER' is not a bare semver (want e.g. 0.5.0, no leading v)" >&2; exit 2; }

if ! git diff --quiet; then
  echo "error: tracked files have uncommitted changes; start from a clean tree" >&2; exit 2
fi
if git rev-parse -q --verify "refs/tags/v$VER" >/dev/null; then
  echo "warning: tag v$VER already exists locally — re-preparing an already-released version?" >&2
fi

export VER
echo "==> fw/Cargo.toml version -> $VER"
perl -pi -e '$done ||= s/^version = "[^"]+"$/version = "$ENV{VER}"/' fw/Cargo.toml
grep -q "^version = \"$VER\"$" fw/Cargo.toml || {
  echo "error: failed to set version in fw/Cargo.toml" >&2; exit 1; }

echo "==> building X4 release image (also refreshes Cargo.lock)"
tools/build-release.sh x4 >/dev/null

FW=target/release-images/firmware.bin
BYTES=$(stat -f%z "$FW" 2>/dev/null || stat -c%s "$FW")
SIZE=$(awk "BEGIN{printf \"%.1f\", $BYTES/1048576}")
echo "==> firmware.bin is $BYTES bytes (~$SIZE MB)"

echo "==> verifying descriptor stamps in the built firmware"
# No `grep -q` here: with pipefail it SIGPIPEs `strings` on early exit and
# fails the pipeline even when the stamp is present.
ELF=target/riscv32imc-unknown-none-elf/release/fw
strings "$ELF" | grep -Fx "$VER" >/dev/null || {
  echo "error: version stamp '$VER' not found in $ELF" >&2; exit 1; }

# The firmware identity gates the OTA bounce (see fw::ota_update): an anchor
# whose descriptor differs in board *or* updater generation is refused, so a
# release has to carry the exact current identity — a matching product, or even
# a matching board prefix, is not enough. Read the expected value from its one
# definition rather than restating it here; a copy in this script would be free
# to drift from the string the firmware stamps and the updater compares.
IDENTITY_SRC=proto/src/ota.rs
EXPECTED_ID=$(sed -n 's/^pub const IDENTITY_X4: &str = "\(.*\)";$/\1/p' "$IDENTITY_SRC")
[[ -n "$EXPECTED_ID" ]] || {
  echo "error: could not read IDENTITY_X4 from $IDENTITY_SRC" >&2; exit 1; }

# Compare the descriptor field the updater actually reads — project_name, the
# 32 bytes at image offset 0x50 — instead of trusting that some matching string
# exists somewhere in the ELF. Exact match, so a stale build, a stray X3 build
# left in the shared target directory, or an older updater generation each fail
# here rather than shipping.
ACTUAL_ID=$(dd if="$FW" bs=1 skip=80 count=32 2>/dev/null | tr -d '\000')
[[ "$ACTUAL_ID" == "$EXPECTED_ID" ]] || {
  echo "error: $FW carries project_name '$ACTUAL_ID', expected '$EXPECTED_ID'" >&2; exit 1; }
echo "==> descriptor identity: $ACTUAL_ID"

echo "==> web/index.html labels -> v$VER, ~$SIZE MB"
export SIZE
perl -pi -e '
  s|(<span class="k">Version</span><b>)v[0-9][^<]*|${1}v$ENV{VER}|;
  s|(<span class="k">Size</span><b>)~[0-9.]+ MB|${1}~$ENV{SIZE} MB|;
  s|v[0-9][0-9.]* release notes|v$ENV{VER} release notes|;
' web/index.html
grep -q "<b>v$VER</b>" web/index.html || {
  echo "error: version label not updated in web/index.html" >&2; exit 1; }

echo
echo "Prepared. Changed files:"
git status --short
echo
cat <<EOF
Next steps (human decides when):
  git add -A && git commit -m "Prepare v$VER release"
  git push origin main
  git tag v$VER && git push origin v$VER      # tag push triggers the release CI

Do NOT create the GitHub release by hand (UI or gh release create): the
release workflow creates it and uploads the assets; a pre-created release
makes that step fail and leaves the release empty, which also blocks the
Pages deploy. Verification steps: docs/agents/release.md.
EOF
