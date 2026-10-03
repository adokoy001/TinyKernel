#!/bin/sh
set -eu
cd "$(dirname "$0")"
QEMU=${QEMU:-qemu-system-x86_64}
if [ ! -f build/tane-os.img ]; then
    python3 build.py
fi
# The data disk for TaneFS. It is created once and kept between runs;
# format it from the shell (admin) the first time.
if [ ! -f build/disk.img ]; then
    python3 -c "open('build/disk.img', 'wb').truncate(1 << 20)"
fi
case "${1:-}" in
    --serial)
        # Ctrl-C reaches the shell. Ctrl-A X exits QEMU's stdio multiplexer.
        exec "$QEMU" -machine pc -accel tcg -m 64M \
            -drive file=build/tane-os.img,format=raw,if=floppy,readonly=on \
            -drive file=build/disk.img,format=raw,if=ide \
            -boot a -netdev user,id=n0,net=10.0.2.0/24,ipv6-net=fd00::/64 \
            -device rtl8139,netdev=n0,romfile= -display none \
            -chardev stdio,id=console,mux=on,signal=off \
            -serial chardev:console -mon chardev=console,mode=readline
        ;;
    "")
        # Click inside the VGA window to type. Ctrl-Alt-G releases capture.
        exec "$QEMU" -machine pc -accel tcg -m 64M \
            -drive file=build/tane-os.img,format=raw,if=floppy,readonly=on \
            -drive file=build/disk.img,format=raw,if=ide \
            -boot a -netdev user,id=n0,net=10.0.2.0/24,ipv6-net=fd00::/64 \
            -device rtl8139,netdev=n0,romfile= -monitor none -serial none
        ;;
    *)
        echo "Usage: sh run.sh [--serial]" >&2
        exit 2
        ;;
esac
