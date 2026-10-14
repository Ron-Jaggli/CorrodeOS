# Runs INSIDE the corrode-dev container. Don't invoke this on the host --
# use ./corrode, which sets the container up correctly.

PROFILE   ?= release
LIMINE_DIR ?= /opt/limine
ISO        := build/corrode.iso
ISO_ROOT   := build/iso_root
MEM        ?= 512M
# Cores handed to the guest. More than one is the point of the SMP code; set
# CPUS=1 to check the kernel still comes up on a uniprocessor.
CPUS       ?= 4
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

# The machine QEMU pretends to be. q35 has PCI Express and an MCFG table;
# MACHINE=pc is the older board with neither, which is how the port-pair
# path to configuration space gets booted at all.
MACHINE ?= q35
# Extra devices for one run, e.g. a bridge with something behind it. Emulated
# ones only: nothing here may name a host device.
QEMU_EXTRA ?=
# Devices every run gets. `edu` is QEMU's teaching device: a megabyte of
# registers and an interrupt it raises when told to, which is exactly enough
# to check that a ring 3 task can be handed a device and nothing else.
QEMU_DEVICES ?= -device edu
# The display card. `std` is QEMU's plain VGA, which the kernel draws on
# through the firmware's framebuffer. `virtio` is a virtio GPU, which a
# driver in ring 3 takes over: the same frames, by another road.
GPU ?= std

QEMU_BASE := -M $(MACHINE) -m $(MEM) -smp $(CPUS) \
             -cdrom $(ISO) -boot d \
             -net none \
             -no-reboot -no-shutdown \
             -vga $(GPU) $(ACCEL) $(QEMU_DEVICES) $(QEMU_EXTRA)

QEMU_COMMON := $(QEMU_BASE) -display none
QEMU_FLAGS  := $(QEMU_COMMON) -serial stdio

# VNC display number; :0 is TCP 5900, which is what ./corrode publishes.
VNC_DISPLAY ?= 0

.PHONY: all kernel user iso run run-gui boot-test fault-test wx-test stack-test user-test screenshot specimen engine-shot check test fmt clippy clean-artifacts

all: iso

kernel:
	cargo build -p corrode-kernel $(CARGO_FLAGS)
	@scripts/check-image.sh $(KERNEL)

# Ring 3 programs: drivers, and the two that exist to be boot checks. Each is
# an ordinary ELF executable, put on the ISO as a Limine module so it is in
# RAM before the kernel starts.
#
# They are built with different flags from the kernel -- their own linker
# script, no kernel code model -- and cargo applies a target's flags to every
# crate built for it. RUSTFLAGS in the environment replaces the ones in
# .cargo/config.toml outright, and a separate target directory stops the two
# sets of flags from invalidating each other's build of the crates they share.
USER_PROGRAMS := edu fault virtio-gpu blob-reader
USER_TARGET   := target/user
USER_BIN      := $(USER_TARGET)/x86_64-unknown-none/release
USER_RUSTFLAGS := -C relocation-model=static -C strip=debuginfo \
                  -C link-arg=-T$(CURDIR)/user/user.ld

user:
	RUSTFLAGS="$(USER_RUSTFLAGS)" cargo build --release --target-dir $(USER_TARGET) \
	    $(foreach program,$(USER_PROGRAMS),-p corrode-$(program))

check:
	cargo check $(CARGO_FLAGS)

clippy:
	cargo clippy $(CARGO_FLAGS) -- -D warnings

# The render and text crates have no kernel in them, so their tests run on the build host
# with std and floating point -- which is the point: the integer animation
# formulas are checked against a float reference they share no code with.
HOST_TARGET := $(shell rustc -vV | sed -n 's/^host: //p')

test:
	cargo test -p corrode-render -p corrode-text -p corrode-gfx -p corrode-pci -p corrode-elf -p corrode-virtio --target $(HOST_TARGET)

# The 3D engine's demo scene, drawn on the build host at each preset, for
# judging by eye. Built optimised, so the times it prints mean something.
engine-shot:
	@mkdir -p build
	@ENGINE_SHOT=$(CURDIR)/build cargo test -q --release -p corrode-gfx \
	    --target $(HOST_TARGET) shot -- --ignored --nocapture
	@for f in build/engine-*.ppm; do \
	    pnmtopng $$f > $${f%.ppm}.png 2>/dev/null && rm -f $$f; \
	done
	@echo "wrote build/engine-*.png"

# A sheet of the built-in typeface, for judging the glyph table by eye.
specimen:
	@mkdir -p build
	@SPECIMEN=$(CURDIR)/build/specimen.pgm cargo test -q -p corrode-text \
	    --target $(HOST_TARGET) specimen -- --ignored > /dev/null
	@pnmtopng build/specimen.pgm > build/specimen.png 2>/dev/null
	@rm -f build/specimen.pgm
	@echo "wrote build/specimen.png"

fmt:
	cargo fmt --all

$(KERNEL): kernel

# A large file for the boot to carry and one task to read: 32 MiB of
# numbered sixteen-byte records, standing in for GPU firmware. Made once; it
# never changes.
TEST_BLOB := build/test.blob
$(TEST_BLOB):
	@mkdir -p build
	seq -f '%015.0f' 0 2097151 > $@

# Firmware, if scripts/fetch-firmware.sh has put any in firmware/. Never
# committed: it is NVIDIA's, under its own licence, and comes from
# linux-firmware. Each file found is one more module.
FIRMWARE := $(shell find firmware -type f -name '*.bin' 2>/dev/null | sort)

iso: $(KERNEL) user $(TEST_BLOB)
	@rm -rf $(ISO_ROOT)/boot/programs $(ISO_ROOT)/boot/firmware
	@mkdir -p $(ISO_ROOT)/boot/limine $(ISO_ROOT)/boot/programs $(ISO_ROOT)/boot/firmware $(ISO_ROOT)/EFI/BOOT
	cp $(KERNEL) $(ISO_ROOT)/boot/corrode
	cp $(foreach program,$(USER_PROGRAMS),$(USER_BIN)/$(program)) $(ISO_ROOT)/boot/programs/
	cp $(TEST_BLOB) $(ISO_ROOT)/boot/firmware/test.blob
	@for file in $(FIRMWARE); do \
	    mkdir -p $(ISO_ROOT)/boot/$$(dirname $$file); cp $$file $(ISO_ROOT)/boot/$$file; \
	done
	@# The configuration is boot/limine.conf plus one line for each module,
	@# so the list of what gets loaded is the list of what was built.
	@{ cat boot/limine.conf; \
	   for program in $(USER_PROGRAMS); do \
	       echo "    module_path: boot():/boot/programs/$$program"; \
	   done; \
	   echo "    module_path: boot():/boot/firmware/test.blob"; \
	   for file in $(FIRMWARE); do \
	       echo "    module_path: boot():/boot/$$file"; \
	   done; } > $(ISO_ROOT)/boot/limine/limine.conf
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
