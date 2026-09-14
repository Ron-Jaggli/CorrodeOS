# Runs INSIDE the corrode-dev container. Don't invoke this on the host --
# use ./corrode, which sets the container up correctly.

PROFILE   ?= release
LIMINE_DIR ?= /opt/limine
ISO        := build/corrode.iso
ISO_ROOT   := build/iso_root
MEM        ?= 512M
BOOT_TIMEOUT ?= 30

ifeq ($(PROFILE),release)
  CARGO_FLAGS := --release
  KERNEL := target/x86_64-unknown-none/release/corrode
else
  CARGO_FLAGS :=
  KERNEL := target/x86_64-unknown-none/debug/corrode
endif

# Cargo features to enable, e.g. FEATURES=fault-test. Feature changes force a
# rebuild on their own, so no cache-busting needed.
FEATURES ?=
ifneq ($(FEATURES),)
  CARGO_FLAGS += --features $(FEATURES)
endif

# KVM if the container was given the device, plain TCG otherwise. Either way
# QEMU sees no host disk, no host network and no passthrough hardware.
ACCEL := $(shell [ -w /dev/kvm ] && echo "-enable-kvm -cpu host" || echo "-cpu qemu64")

QEMU_BASE := -M q35 -m $(MEM) \
             -cdrom $(ISO) -boot d \
             -net none \
             -no-reboot -no-shutdown \
             $(ACCEL)

QEMU_COMMON := $(QEMU_BASE) -display none
QEMU_FLAGS  := $(QEMU_COMMON) -serial stdio

# VNC display number; :0 is TCP 5900, which is what ./corrode publishes.
VNC_DISPLAY ?= 0

.PHONY: all kernel iso run run-gui boot-test fault-test wx-test stack-test user-test screenshot check fmt clippy clean-artifacts

all: iso

kernel:
	cargo build $(CARGO_FLAGS)
	@scripts/check-image.sh $(KERNEL)

check:
	cargo check $(CARGO_FLAGS)

clippy:
	cargo clippy $(CARGO_FLAGS) -- -D warnings

fmt:
	cargo fmt --all

$(KERNEL): kernel

iso: $(KERNEL)
	@mkdir -p $(ISO_ROOT)/boot/limine $(ISO_ROOT)/EFI/BOOT
	cp $(KERNEL) $(ISO_ROOT)/boot/corrode
	cp boot/limine.conf $(ISO_ROOT)/boot/limine/
	cp $(LIMINE_DIR)/limine-bios.sys \
	   $(LIMINE_DIR)/limine-bios-cd.bin \
	   $(LIMINE_DIR)/limine-uefi-cd.bin $(ISO_ROOT)/boot/limine/
	cp $(LIMINE_DIR)/BOOTX64.EFI $(LIMINE_DIR)/BOOTIA32.EFI $(ISO_ROOT)/EFI/BOOT/
	xorriso -as mkisofs -quiet -R -r -J \
	    -b boot/limine/limine-bios-cd.bin \
	    -no-emul-boot -boot-load-size 4 -boot-info-table \
	    --efi-boot boot/limine/limine-uefi-cd.bin \
	    -efi-boot-part --efi-boot-image \
	    --protective-msdos-label \
	    $(ISO_ROOT) -o $(ISO)
	$(LIMINE_DIR)/limine bios-install $(ISO)
	@echo "built $(ISO)"

# Interactive boot. Ctrl-A X to quit QEMU.
run: iso
	qemu-system-x86_64 $(QEMU_FLAGS)

# Live framebuffer over VNC, serial still on stdout.
#
# QEMU binds the VNC socket on all interfaces *inside the container*, because
# podman's published-port forwarding arrives on the container's veth, not its
# loopback. Exposure is clamped on the host side instead: ./corrode publishes it
# to 127.0.0.1 only, so nothing outside this machine can reach it.
run-gui: iso
	@echo "VNC listening -- connect to 127.0.0.1:590$(VNC_DISPLAY)"
	qemu-system-x86_64 $(QEMU_BASE) -vnc :$(VNC_DISPLAY) -serial stdio

# Non-interactive boot for CI and for the agent loop: capture serial, give up
# after BOOT_TIMEOUT seconds (the kernel halts rather than exiting, so a
# timeout is the expected way this ends, not a failure).
boot-test: iso
	@echo "booting for $(BOOT_TIMEOUT)s, serial -> build/serial.log"
	@timeout --foreground $(BOOT_TIMEOUT) \
	    qemu-system-x86_64 $(QEMU_FLAGS) < /dev/null \
	    | tee build/serial.log; \
	  status=$$?; \
	  if [ $$status -ne 0 ] && [ $$status -ne 124 ]; then exit $$status; fi
	@echo "--- serial log captured ---"

# Boot, let the kernel settle, then dump the framebuffer through the QEMU
# monitor. This is how we verify the renderer actually put pixels where it
# claims -- serial output only proves the code ran.
screenshot: iso
	@rm -f build/frame.ppm build/frame.png
	@( sleep $(SHOT_DELAY); echo "screendump build/frame.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 60 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@test -s build/frame.ppm || { echo "screendump produced nothing"; exit 1; }
	@pnmtopng build/frame.ppm > build/frame.png 2>/dev/null
	@rm -f build/frame.ppm
	@echo "wrote build/frame.png"

SHOT_DELAY ?= 6

# Boot with the deliberate page fault enabled and capture what the handler says.
fault-test:
	$(MAKE) FEATURES=fault-test boot-test

# Prove W^X: writing to .text must trap.
wx-test:
	$(MAKE) FEATURES=wx-test boot-test

# Prove stack guard pages: overflowing a task stack must trap.
stack-test:
	$(MAKE) FEATURES=stack-test boot-test

# Prove the ring boundary: a ring 3 task reading kernel memory must trap.
user-test:
	$(MAKE) FEATURES=user-test boot-test

# Only ever removes build output this Makefile created.
clean-artifacts:
	rm -rf build target
