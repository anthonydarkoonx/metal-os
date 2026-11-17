#!/bin/sh
# Boot this copy of Metal OS with the desktop (../docs/DESKTOP.md). Build first: make user disk && (cd kernel && cargo build
# --release --features park-on-exit --target-dir target-window). `./run-desktop.sh` opens a window; `./run-desktop.sh headless DIR`
# runs without one, the serial log in DIR/serial.log and QEMU's control socket at DIR/qmp.sock (for screenshots and input).
cd "$(dirname "$0")"
RAM=128
if [ "$1" = headless ]; then
  OUT=${2:-.}
  rm -f "$OUT/qmp.sock"
  DISPLAY_ARGS="-display none -serial file:$OUT/serial.log -monitor none -qmp unix:$OUT/qmp.sock,server,nowait"
else
  DISPLAY_ARGS="-display gtk,zoom-to-fit=on -serial vc:900x600 -monitor none -qmp unix:build-window.sock,server,nowait"
fi
exec qemu-system-aarch64 -machine virt,gic-version=2,iommu=smmuv3 -cpu max -smp 4 -m ${RAM}M \
  -fw_cfg name=opt/metal/ram-bytes,string=$((RAM * 1048576)) -fw_cfg name=opt/metal/interactive,string=1 \
  $DISPLAY_ARGS -semihosting -name "Metal OS (desktop)" \
  -drive if=none,id=d0,file=disk.img,format=raw -device virtio-blk-pci,drive=d0,disable-legacy=on,iommu_platform=on \
  -device virtio-gpu-pci,disable-legacy=on,iommu_platform=on,xres=1024,yres=768 \
  -device virtio-keyboard-pci,disable-legacy=on,iommu_platform=on -device virtio-tablet-pci,disable-legacy=on,iommu_platform=on \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0,disable-legacy=on,iommu_platform=on \
  -device virtio-rng-pci,disable-legacy=on,iommu_platform=on \
  -kernel kernel/target-window/aarch64-unknown-none-softfloat/release/metal-kernel
