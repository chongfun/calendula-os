#!/usr/bin/env bash
# Build distributable firmware images for one Xteink board target.
#
# Produces, in target/release-images/<board>/:
#   firmware.bin    app image for OTA slot app0/ota_0. Flash to 0x10000. This is
#                   what the web flasher, `esptool write_flash 0x10000`, and the
#                   in-app SD/OTA updater consume. Leaves the bootloader intact.
#   update.bin      byte-identical to firmware.bin, under the filename the stock
#                   OEM SD-card updater looks for on a locked unit's card. The
#                   OEM updater writes it to the app slot (0x10000) — it is an
#                   app image, NOT a full-flash image.
#   full-flash.bin  merged 16 MB image (bootloader + partition table + app) for
#                   programming a whole *unlocked* unit from scratch with
#                   `esptool write_flash 0x0`. NEVER put this on an SD card and
#                   NEVER write it to 0x10000 — it would land a bootloader in the
#                   app slot and brick the device.
#
# firmware.bin/update.bin carry our app descriptor (magic 0xABCD5432 at image
# offset 0x20) with the wide-open eFuse-revision range, which is what lets the
# stock bootloader on a locked unit accept a non-stock image.
#
# Usage: tools/build-release.sh x4|x3
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ $# -ne 1 ]]; then
    echo "usage: $0 x4|x3" >&2
    exit 2
fi

BOARD=$1
case "$BOARD" in
    x4)
        BOARD_FEATURES=(--no-default-features --features board-x4)
        ;;
    x3)
        BOARD_FEATURES=(--no-default-features --features board-x3)
        ;;
    *)
        echo "unknown board '$BOARD'; expected x4 or x3" >&2
        exit 2
        ;;
esac

CHIP=esp32c3
FLASH_SIZE=16mb
PARTS=partitions.csv
APP_LABEL=app0                # the ota_0 partition's label in partitions.csv
ELF=target/riscv32imc-unknown-none-elf/release/fw
OUT="target/release-images/$BOARD"

echo "==> building fw for $BOARD (release)"
cargo build -p fw --release "${BOARD_FEATURES[@]}"

mkdir -p "$OUT"

# espflash validates the app descriptor against its own schema and rejects our
# hand-rolled one; --ignore-app-descriptor skips that check. The descriptor is
# still present and correctly placed at image offset 0x20 for the bootloader.
common=(--chip "$CHIP" --flash-size "$FLASH_SIZE"
        --partition-table "$PARTS" --target-app-partition "$APP_LABEL"
        --ignore-app-descriptor)

echo "==> firmware.bin (app image, app0/ota_0 @ 0x10000)"
espflash save-image "${common[@]}" "$ELF" "$OUT/firmware.bin"

echo "==> update.bin (same app image, name the OEM SD updater reads)"
cp "$OUT/firmware.bin" "$OUT/update.bin"

echo "==> full-flash.bin (merged 16 MB, unlocked-only, write to 0x0)"
espflash save-image "${common[@]}" --merge "$ELF" "$OUT/full-flash.bin"

echo
echo "Artifacts in $OUT:"
ls -la "$OUT/firmware.bin" "$OUT/update.bin" "$OUT/full-flash.bin"
echo
echo "Flash paths (see docs/FLASHING.md):"
if [[ "$BOARD" == x3 ]]; then
    echo "  Locked X3            : stock SD-updater compatibility is not hardware-validated;"
    echo "                         do not use this path without accepting brick risk."
else
    echo "  Locked (stock updater): copy update.bin to the SD card root, then power"
    echo "                          on holding Power + Up on USB power."
fi
echo "  Unlocked, app only    : esptool.py --chip $CHIP write_flash 0x10000 $OUT/firmware.bin"
echo "  Unlocked, whole flash : esptool.py --chip $CHIP write_flash 0x0 $OUT/full-flash.bin"
