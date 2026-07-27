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
OUT_X3=target/release-images/firmware-x3.bin
BYTES=$(stat -f%z "$FW" 2>/dev/null || stat -c%s "$FW")
SIZE=$(awk "BEGIN{printf \"%.1f\", $BYTES/1048576}")
echo "==> firmware.bin is $BYTES bytes (~$SIZE MB)"

echo "==> verifying descriptor stamps in the built firmware"
# Read a fixed-width app-descriptor field out of the image, truncated at the
# first NUL — the rule proto::ota::project_name() applies when the updater
# compares one. Checking the field beats scanning the ELF for a matching string:
# a stale build, or a stray X3 build left in the shared target directory, fails
# here instead of passing on a string that merely exists somewhere.
# (Converting NULs to newlines and cutting at the first one, rather than piping
# to `head`, keeps pipefail from tripping over a SIGPIPE.)
desc_field() { # <image> <byte offset> <field width>
  local raw
  raw=$(dd if="$1" bs=1 skip="$2" count="$3" 2>/dev/null | tr '\000' '\n')
  printf '%s' "${raw%%$'\n'*}"
}

# The firmware identity gates the OTA bounce (see fw::ota_update): an anchor
# whose descriptor differs in board *or* updater generation is refused, so a
# release has to carry the exact current identity — a matching product, or even
# a matching board prefix, is not enough. Read the expected value from its one
# definition rather than restating it here; a copy in this script would be free
# to drift from the string the firmware stamps and the updater compares.
IDENTITY_SRC=proto/src/ota.rs
expected_identity() { # <const name>
  local v
  v=$(sed -n "s/^pub const $1: &str = \"\(.*\)\";\$/\1/p" "$IDENTITY_SRC")
  [[ -n "$v" ]] || { echo "error: could not read $1 from $IDENTITY_SRC" >&2; exit 1; }
  printf '%s' "$v"
}

# `version` is the 32 bytes at image offset 0x30 (stamped from
# env!("CARGO_PKG_VERSION")) and `project_name` the 32 at 0x50 — the exact field
# anchor_can_apply_update() compares. Both are read out of the image rather than
# scanned for in the ELF, so a stale build, a stray build for the other board,
# or an older updater generation fails here instead of shipping.
verify_image() { # <image> <expected identity>
  local ver id
  ver=$(desc_field "$1" 48 32)
  [[ "$ver" == "$VER" ]] || {
    echo "error: $1 carries version '$ver', expected '$VER'" >&2; exit 1; }
  id=$(desc_field "$1" 80 32)
  [[ "$id" == "$2" ]] || {
    echo "error: $1 carries project_name '$id', expected '$2'" >&2; exit 1; }
  echo "==> $1: v$ver, $id"
}

verify_image "$FW" "$(expected_identity IDENTITY_X4)"

# The release ships both boards (see .github/workflows/release.yml), and the X3
# image carries its own identity that the X3 updater compares just as strictly.
# Build it here too rather than trusting whatever firmware-x3.bin a previous run
# left in the target directory — a stale one would pass this check while the
# release CI built something else.
echo "==> building X3 release image to verify its stamps"
tools/build-release.sh x3 >/dev/null
verify_image "$OUT_X3" "$(expected_identity IDENTITY_X3)"

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
