#!/bin/sh
set -eu
cd "$(dirname "$0")"
if [ ! -f build/tane-os.img ]; then
    python3 build.py
fi
case "${1:-}" in
    --serial)
        # Ctrl-C exits QEMU; guest input arrives through our COM1 driver.
        exec qemu-system-x86_64 -machine pc -accel tcg -m 64M \
            -drive file=build/tane-os.img,format=raw,if=floppy,readonly=on \
            -boot a -nic none -display none -monitor none -serial stdio
        ;;
    "")
        # Click inside the VGA window to type. Ctrl-Alt-G releases capture.
        exec qemu-system-x86_64 -machine pc -accel tcg -m 64M \
            -drive file=build/tane-os.img,format=raw,if=floppy,readonly=on \
            -boot a -nic none -monitor none -serial none
        ;;
    *)
        echo "Usage: sh run.sh [--serial]" >&2
        exit 2
        ;;
esac
