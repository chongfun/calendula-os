#!/usr/bin/env bash
# Build the distributable firmware images for the Xteink X4, X3, or X4 Pro.
#
# Usage: tools/build-release.sh [x4|x3|x4-pro]   (default x4)
#
# Produces local development images in target/release-images/:
#   firmware.bin         X4 app image for OTA slot app0/ota_0 (flash to 0x10000).
#   firmware-x3.bin      X3 app image for OTA slot app0/ota_0 (flash to 0x10000).
#   firmware-x4-pro.bin  X4 Pro (ESP32-S3) app image for app0/app1 OTA slots.
#   update.bin           X4 stock/OEM SD updater filename.
#   update-x3.bin        X3 stock/OEM SD updater image; rename to update.bin on card.
#   update-x4-pro.bin    X4 Pro SD updater artifact (same app image).
#   FWUPDATE.BIN         X4 in-app SD updater filename.
#   FWUPDX3.BIN          X3 in-app SD updater filename.
#   FWUPDX4P.BIN         X4 Pro in-app SD updater filename.
#   full-flash*.bin      merged 16 MB image (bootloader + partition table + app)
#                        for programming from 0x0. NEVER put this on an SD card.
#
# GitHub releases publish app/SD images: firmware-x4.bin, firmware-x3.bin,
# firmware-x4-pro.bin, update.bin, update-x3.bin, FWUPDATE.BIN, FWUPDX3.BIN,
# and FWUPDX4P.BIN. full-flash*.bin remains local-only.
#
# The app images carry our app descriptor (magic 0xABCD5432 at image offset
# 0x20) with the wide-open eFuse-revision range, which is what lets the stock
# bootloader on a locked unit accept a non-stock image.
set -euo pipefail

cd "$(dirname "$0")/.."

# Local packaging and CI must use the same espflash version. Its output
# bytes are part of the published OTA/SD image format, not just a host detail.
expected_espflash="$(tr -d '[:space:]' < tools/espflash-version)"
actual_espflash="$(espflash --version)"
if [[ "$actual_espflash" != "espflash $expected_espflash" ]]; then
  echo "error: expected espflash $expected_espflash, found $actual_espflash" >&2
  exit 1
fi

# STOCK_SD_ARTIFACT is the unique release filename; STOCK_SD_CARD_NAME is the
# name expected at the card root. X3 gets a unique artifact so it cannot
# overwrite the X4 update.bin when both builds share the release directory.
# IN_APP_SD_IMAGE is the trigger filename Calendula itself consumes on boot.
DEVICE="${1:-x4}"
case "$DEVICE" in
  x4)
    FEATURES=()
    SUFFIX=""
    STOCK_SD_ARTIFACT=update.bin
    STOCK_SD_CARD_NAME=update.bin
    IN_APP_SD_IMAGE=FWUPDATE.BIN
    CHIP=esp32c3
    PARTS=partitions.csv
    ELF=target/riscv32imc-unknown-none-elf/release/fw-c3
    ;;
  x3)
    FEATURES=(--features device-x3)
    SUFFIX="-x3"
    STOCK_SD_ARTIFACT=update-x3.bin
    STOCK_SD_CARD_NAME=update.bin
    IN_APP_SD_IMAGE=FWUPDX3.BIN
    CHIP=esp32c3
    PARTS=partitions.csv
    ELF=target/riscv32imc-unknown-none-elf/release/fw-c3
    ;;
  x4-pro)
    FEATURES=()
    SUFFIX="-x4-pro"
    STOCK_SD_ARTIFACT=update-x4-pro.bin
    STOCK_SD_CARD_NAME=update.bin
    IN_APP_SD_IMAGE=FWUPDX4P.BIN
    CHIP=esp32s3
    PARTS=partitions-x4-pro.csv
    ELF=fw-s3/target/xtensa-esp32s3-none-elf/release/fw-s3
    ;;
  *) echo "usage: $0 [x4|x3|x4-pro]" >&2; exit 2 ;;
esac

FLASH_SIZE=16mb
APP_LABEL=app0                # the ota_0 partition's label in the partition table
OUT=target/release-images
FW="$OUT/firmware$SUFFIX.bin"
FULL="$OUT/full-flash$SUFFIX.bin"

if [[ "$DEVICE" == "x4-pro" ]]; then
  echo "==> building fw-s3 ($DEVICE, release)"
  tools/cargo-s3.sh build --release
else
  echo "==> building fw-c3 ($DEVICE, release)"
  if ((${#FEATURES[@]})); then
    tools/cargo-c3.sh build -p fw-c3 --release "${FEATURES[@]}"
  else
    tools/cargo-c3.sh build -p fw-c3 --release
  fi
fi

mkdir -p "$OUT"

# espflash validates the app descriptor against its own schema and rejects our
# hand-rolled one; --ignore-app-descriptor skips that check. The descriptor is
# still present and correctly placed at image offset 0x20 for the bootloader.
# Dual I/O at 80 MHz instead of espflash's 40 MHz default: it sets the clock in
# both the bootloader for full-flash images and the app image header for app-only/SD
# updates (which the ESP-IDF bootloader reads to configure flash at boot).
common=(--chip "$CHIP" --flash-size "$FLASH_SIZE"
        --flash-mode dio --flash-freq 80mhz
        --partition-table "$PARTS" --target-app-partition "$APP_LABEL"
        --ignore-app-descriptor)

echo "==> firmware$SUFFIX.bin (app image, app0/ota_0 @ 0x10000)"
espflash save-image "${common[@]}" "$ELF" "$FW"

echo "==> $STOCK_SD_ARTIFACT (same app image, stock/OEM SD updater artifact)"
cp "$FW" "$OUT/$STOCK_SD_ARTIFACT"
if [[ "$IN_APP_SD_IMAGE" != "$STOCK_SD_ARTIFACT" ]]; then
  echo "==> $IN_APP_SD_IMAGE (same app image, Calendula in-app SD updater name)"
  cp "$FW" "$OUT/$IN_APP_SD_IMAGE"
fi

echo "==> full-flash$SUFFIX.bin (merged 16 MB, unlocked-only, write to 0x0)"
espflash save-image "${common[@]}" --merge "$ELF" "$FULL"

echo
echo "Artifacts in $OUT:"
artifacts=("$FW" "$OUT/$STOCK_SD_ARTIFACT" "$FULL")
if [[ "$IN_APP_SD_IMAGE" != "$STOCK_SD_ARTIFACT" ]]; then
  artifacts+=("$OUT/$IN_APP_SD_IMAGE")
fi
ls -la "${artifacts[@]}"
echo
echo "Flash paths (see docs/FLASHING.md):"
echo "  Calendula SD update: copy $IN_APP_SD_IMAGE to the SD card root, then reboot."
echo "  Stock/OEM updater : copy $STOCK_SD_ARTIFACT as $STOCK_SD_CARD_NAME to the SD card root, then power"
echo "                      on holding Power + Up on USB power."
echo "  Unlocked, app only: esptool.py --chip $CHIP write_flash 0x10000 $FW"
echo "  Unlocked, whole   : esptool.py --chip $CHIP write_flash 0x0 $FULL"
