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


.PHONY: input-test terminal-test close-test dock-test minimize-test settings-test churn-test shortcut-test resize-test files-test scrollback-test switcher-test cursor-test search-test reveal-test notify-test control-test lock-test snap-test history-test keys-test menu-test dock-menu-test title-test dock-windows-test double-click-test tile-test show-desktop-test shell-test magnify-test magnify-setting-test
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
USER_PROGRAMS := edu fault virtio-gpu blob-reader nvidia ps2 terminal panel settings about files control lock menu
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
	cargo test -p corrode-render -p corrode-text -p corrode-gfx -p corrode-pci -p corrode-elf -p corrode-virtio -p corrode-nv -p corrode-input -p corrode-heap -p corrode-line -p corrode-shell -p corrode-theme --target $(HOST_TARGET)

# What a title bar of glass costs the software renderer, built optimised.
glass-bench:
	cargo test --release -p corrode-render --target $(HOST_TARGET) glass_bench -- --ignored --nocapture

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

# The files a terminal can read: root/, packed as a ustar archive the
# bootloader loads like any other module. Sorted, with owners and times
# fixed, so the same tree always makes the same bytes.
ROOT_TAR := build/root.tar
ROOT_FILES := $(shell find root 2>/dev/null)
$(ROOT_TAR): $(ROOT_FILES)
	@mkdir -p build
	tar --format=ustar --sort=name --owner=0 --group=0 --numeric-owner \
	    --mtime=@0 --mode=u+rw,go+r,go-w -C root -cf $@ .

iso: $(KERNEL) user $(TEST_BLOB) $(ROOT_TAR)
	@rm -rf $(ISO_ROOT)/boot/programs $(ISO_ROOT)/boot/firmware $(ISO_ROOT)/boot/files
	@mkdir -p $(ISO_ROOT)/boot/limine $(ISO_ROOT)/boot/programs $(ISO_ROOT)/boot/firmware $(ISO_ROOT)/boot/files $(ISO_ROOT)/EFI/BOOT
	cp $(KERNEL) $(ISO_ROOT)/boot/corrode
	cp $(foreach program,$(USER_PROGRAMS),$(USER_BIN)/$(program)) $(ISO_ROOT)/boot/programs/
	cp $(TEST_BLOB) $(ISO_ROOT)/boot/firmware/test.blob
	cp $(ROOT_TAR) $(ISO_ROOT)/boot/files/root.tar
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
	   echo "    module_path: boot():/boot/files/root.tar"; \
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

# Boot, then type and move the mouse through the QEMU monitor, as a person
# at the keyboard would: the PS/2 driver in ring 3 hears it, and the serial
# log and build/input.png show what reached the screen. QEMU's `sendkey`
# and `mouse_move` drive the emulated controller, not the host's devices.
INPUT_KEYS ?= shift-h i spc t h e r e ret
input-test: iso
	@rm -f build/input.ppm build/input.png
	@( sleep $(SHOT_DELAY); \
	   for key in $(INPUT_KEYS); do echo "sendkey $$key"; sleep 0.2; done; \
	   echo "mouse_move 120 -80"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 0.3; \
	   echo "sendkey o"; sleep 0.2; echo "sendkey k"; sleep 0.5; \
	   echo "mouse_move -250 -150"; sleep 0.2; echo "mouse_move -250 -150"; sleep 0.2; \
	   echo "mouse_move -80 0"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   for key in h e l p ret e c h o spc w i n d o w ret m e m ret; do \
	       echo "sendkey $$key"; sleep 0.15; done; sleep 0.5; \
	   echo "mouse_move 200 70"; sleep 0.3; \
	   echo "mouse_button 1"; sleep 0.3; \
	   echo "mouse_move 150 120"; sleep 0.3; echo "mouse_move 150 120"; sleep 0.3; \
	   echo "mouse_button 0"; sleep 1; \
	   echo "screendump build/input.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^(input|ps2|panel|terminal) " build/serial.log || echo "input-test: nothing from the input path"
	@test -s build/input.ppm && pnmtopng build/input.ppm > build/input.png 2>/dev/null && rm -f build/input.ppm && echo "wrote build/input.png" || true

# Open Files from the dock, then work it from the keyboard: down twice to
# etc, Return to open it, down to desktop.conf. build/files.png shows its
# preview, scrolled down nine lines by three turns of the wheel.
files-test: iso
	@rm -f build/files.ppm build/files.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -84 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 2; \
	   for key in down down ret down; do echo "sendkey $$key"; sleep 0.6; done; \
	   echo "mouse_move 150 -300"; sleep 0.5; \
	   for n in 1 2 3; do echo "mouse_move 0 0 -1"; sleep 0.4; done; \
	   echo "screendump build/files.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^(dock|files) " build/serial.log || echo "files-test: nothing from Files"
	@grep -q "^files      : /etc (3 items)" build/serial.log \
	    && echo "files-test: opened /etc from the keyboard            PASS" \
	    || echo "files-test: opened /etc from the keyboard            FAIL"
	@grep -q "now says Files - etc" build/serial.log \
	    && echo "files-test: the title bar names the folder           PASS" \
	    || echo "files-test: the title bar names the folder           FAIL"
	@test -s build/files.ppm && pnmtopng build/files.ppm > build/files.png 2>/dev/null && rm -f build/files.ppm && echo "wrote build/files.png" || true

# Open a terminal from the dock, fill it with help twice, then turn the
# wheel back four notches. build/scrollback.png shows older output with a
# scroller.
scrollback-test: iso
	@rm -f build/scrollback.ppm build/scrollback.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   for round in 1 2; do for key in h e l p ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 0.5; done; \
	   echo "mouse_move 0 -420"; sleep 0.5; \
	   for n in 1 2 3 4; do echo "mouse_move 0 0 1"; sleep 0.3; done; sleep 0.5; \
	   echo "screendump build/scrollback.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^terminal .*lines back" build/serial.log || echo "scrollback-test: the terminal never scrolled"
	@grep -q "^terminal   : 12 lines back" build/serial.log \
	    && echo "scrollback-test: the wheel scrolled the terminal back  PASS" \
	    || echo "scrollback-test: the wheel scrolled the terminal back  FAIL"
	@test -s build/scrollback.ppm && pnmtopng build/scrollback.ppm > build/scrollback.png 2>/dev/null && rm -f build/scrollback.ppm && echo "wrote build/scrollback.png" || true

# Open Files, Terminal and Settings from the dock, then Alt+Tab twice. The
# first is a plain tap: letting go of Alt at once brings Terminal -- the
# window behind Settings -- to the front. The second is held, and
# build/switcher.png shows the panel mid-hold. (QEMU sends no keys at all
# after one held with `sendkey ... HOLD`, so the held one comes last.)
switcher-test: iso
	@rm -f build/switcher.ppm build/switcher.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -84 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   for step in 1 2; do echo "mouse_move 56 0"; sleep 0.3; \
	     echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; done; \
	   echo "mouse_move 300 -300"; sleep 0.5; \
	   echo "sendkey alt-tab"; sleep 1; \
	   echo "sendkey alt-tab 3000"; sleep 5; \
	   echo "screendump build/switcher.ppm"; sleep 1.5; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^desktop .*switch" build/serial.log || echo "switcher-test: no switcher"
	@grep -q "^desktop    : switched to window 2" build/serial.log \
	    && test "$$(grep -c '^desktop    : switcher up, 3 windows' build/serial.log)" = 2 \
	    && echo "switcher-test: Alt+Tab showed three and switched     PASS" \
	    || echo "switcher-test: Alt+Tab showed three and switched     FAIL"
	@test -s build/switcher.ppm && pnmtopng build/switcher.ppm > build/switcher.png 2>/dev/null && rm -f build/switcher.ppm && echo "wrote build/switcher.png" || true

# Open a terminal from the dock and hover its right edge, then its
# bottom-right corner: the pointer turns into resize arrows, the corner's
# in build/cursor.png.
cursor-test: iso
	@rm -f build/cursor.ppm build/cursor.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "mouse_move 104 -229"; sleep 0.2; echo "mouse_move 0 -229"; sleep 0.8; \
	   echo "mouse_move 0 154"; sleep 0.8; \
	   echo "screendump build/cursor.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "pointer is now" build/serial.log || echo "cursor-test: the pointer never changed"
	@grep -q "pointer is now Across" build/serial.log \
	    && grep -q "pointer is now Falling" build/serial.log \
	    && echo "cursor-test: resize arrows over an edge and a corner  PASS" \
	    || echo "cursor-test: resize arrows over an edge and a corner  FAIL"
	@test -s build/cursor.ppm && pnmtopng build/cursor.ppm > build/cursor.png 2>/dev/null && rm -f build/cursor.ppm && echo "wrote build/cursor.png" || true

# Alt+Space for search, type "s" -- Settings, which starts with it, then
# Files, which has one -- and press Return: Settings opens.
# build/search.png shows the field before Return.
search-test: iso
	@rm -f build/search.ppm build/search.png
	@( sleep $(SHOT_DELAY); \
	   echo "sendkey alt-spc"; sleep 0.8; echo "sendkey s"; sleep 0.8; \
	   echo "screendump build/search.ppm"; sleep 1.5; \
	   echo "sendkey ret"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^desktop .*search|^dock .*Settings" build/serial.log || echo "search-test: no search"
	@grep -q 'search "s" opens Settings' build/serial.log \
	    && grep -q "dock       : started Settings" build/serial.log \
	    && echo "search-test: Alt+Space, s, Return opened Settings    PASS" \
	    || echo "search-test: Alt+Space, s, Return opened Settings    FAIL"
	@test -s build/search.ppm && pnmtopng build/search.ppm > build/search.png 2>/dev/null && rm -f build/search.ppm && echo "wrote build/search.png" || true

# Alt+Space, type "mot" and press Return: Files opens on /etc with motd
# picked and previewed, in build/reveal.png.
reveal-test: iso
	@rm -f build/reveal.ppm build/reveal.png
	@( sleep $(SHOT_DELAY); \
	   echo "sendkey alt-spc"; sleep 0.8; \
	   for key in m o t; do echo "sendkey $$key"; sleep 0.4; done; \
	   echo "sendkey ret"; sleep 2.5; \
	   echo "screendump build/reveal.ppm"; sleep 1.5; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^desktop .*search|^files " build/serial.log || echo "reveal-test: no search"
	@grep -q "^files      : showing /etc/motd" build/serial.log \
	    && echo "reveal-test: search found motd and Files showed it   PASS" \
	    || echo "reveal-test: search found motd and Files showed it   FAIL"
	@test -s build/reveal.ppm && pnmtopng build/reveal.ppm > build/reveal.png 2>/dev/null && rm -f build/reveal.ppm && echo "wrote build/reveal.png" || true

# Open a terminal from the dock and run notify twice. The first banner
# slides in at the top right and leaves by itself after four seconds; the
# second, in build/notify.png, is clicked away.
notify-test: iso
	@rm -f build/notify.ppm build/notify.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   for key in n o t i f y spc s t a r t e d ret; do echo "sendkey $$key"; sleep 0.1; done; \
	   sleep 5.5; \
	   for key in n o t i f y spc b u i l d spc d o n e ret; do echo "sendkey $$key"; sleep 0.1; done; \
	   sleep 1.2; echo "screendump build/notify.ppm"; sleep 1; \
	   echo "mouse_move 230 -350"; sleep 0.2; echo "mouse_move 200 -340"; sleep 0.3; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^notify |notification" build/serial.log || echo "notify-test: no notification"
	@grep -q "^notify     : Terminal: build done" build/serial.log \
	    && grep -q "notification from Terminal shown" build/serial.log \
	    && grep -q "notification dismissed" build/serial.log \
	    && test "$$(grep -c 'notification gone' build/serial.log)" = 2 \
	    && echo "notify-test: one banner timed out, one was clicked   PASS" \
	    || echo "notify-test: one banner timed out, one was clicked   FAIL"
	@test -s build/notify.ppm && pnmtopng build/notify.ppm > build/notify.png 2>/dev/null && rm -f build/notify.ppm && echo "wrote build/notify.png" || true

# Click the menu bar clock for Control Center, turn Dark Mode off from it
# (build/control.png), then click the clock again: it closes.
control-test: iso
	@rm -f build/control.ppm build/control.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move 280 -193"; sleep 0.2; echo "mouse_move 280 -193"; sleep 0.4; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "mouse_move -155 60"; sleep 0.4; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "screendump build/control.ppm"; sleep 1; \
	   echo "mouse_move 155 -60"; sleep 0.4; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^control |popover|Control Center" build/serial.log || echo "control-test: no Control Center"
	@grep -q "^control    : Dark Mode turned Off" build/serial.log \
	    && grep -q "^control    : closed" build/serial.log \
	    && test "$$(grep -c '^control    : open' build/serial.log)" = 1 \
	    && echo "control-test: clock opened it, toggled, clock closed it PASS" \
	    || echo "control-test: clock opened it, toggled, clock closed it FAIL"
	@test -s build/control.ppm && pnmtopng build/control.ppm > build/control.png 2>/dev/null && rm -f build/control.ppm && echo "wrote build/control.png" || true

# Open a terminal, lock with Alt+L, try Alt+Tab and Alt+Space under the
# lock (neither may answer), screenshot it (build/lock.png), then Return
# unlocks. Then Alt+L again and unlock by clicking the button.
lock-test: iso
	@rm -f build/lock.ppm build/lock.png build/lock-fade.ppm build/lock-fade.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "sendkey alt-l"; sleep 1.5; \
	   echo "sendkey alt-tab"; sleep 0.3; echo "sendkey alt-spc"; sleep 0.3; echo "sendkey x"; sleep 0.8; \
	   echo "screendump build/lock.ppm"; sleep 1; \
	   echo "sendkey ret"; sleep 0.06; echo "screendump build/lock-fade.ppm"; sleep 1.2; \
	   echo "sendkey alt-l"; sleep 1.5; \
	   echo "mouse_move 0 -60"; sleep 0.4; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^lock |covered|switcher up|search up" build/serial.log || echo "lock-test: no lock screen"
	@test "$$(grep -c '^lock       : locked' build/serial.log)" = 2 \
	    && test "$$(grep -c '^lock       : unlocked' build/serial.log)" = 2 \
	    && ! grep -q "switcher up\|search up" build/serial.log \
	    && echo "lock-test: locked twice, shortcuts held off, unlocked twice PASS" \
	    || echo "lock-test: locked twice, shortcuts held off, unlocked twice FAIL"
	@test -s build/lock.ppm && pnmtopng build/lock.ppm > build/lock.png 2>/dev/null && rm -f build/lock.ppm && echo "wrote build/lock.png" || true
	@test -s build/lock-fade.ppm && pnmtopng build/lock-fade.ppm > build/lock-fade.png 2>/dev/null && rm -f build/lock-fade.ppm && echo "wrote build/lock-fade.png" || true

# Open a terminal and drag it by its bar to the left edge: a preview
# shows the left half (build/snap.png), and letting go snaps it there.
# Then drag it to the right edge and let go: the right half.
snap-test: iso
	@rm -f build/snap.ppm build/snap.png build/snapped.ppm build/snapped.png build/unsnapped.ppm build/unsnapped.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "mouse_move -64 -218"; sleep 0.2; echo "mouse_move -64 -218"; sleep 0.2; echo "mouse_move -64 -219"; sleep 0.4; \
	   echo "mouse_button 1"; sleep 0.3; \
	   echo "mouse_move -200 0"; sleep 0.2; echo "mouse_move -200 0"; sleep 0.2; echo "mouse_move -60 0"; sleep 0.8; \
	   echo "screendump build/snap.ppm"; sleep 0.8; \
	   echo "mouse_button 0"; sleep 1.2; \
	   echo "mouse_move 300 -49"; sleep 0.4; \
	   echo "mouse_button 1"; sleep 0.3; \
	   for step in 1 2 3 4 5; do echo "mouse_move 220 0"; sleep 0.2; done; sleep 0.6; \
	   echo "mouse_button 0"; sleep 1.2; \
	   echo "screendump build/snapped.ppm"; sleep 1; \
	   echo "mouse_move -300 0"; sleep 0.4; \
	   echo "mouse_button 1"; sleep 0.3; \
	   echo "mouse_move -200 120"; sleep 0.2; echo "mouse_move -100 60"; sleep 0.6; \
	   echo "mouse_button 0"; sleep 1.2; \
	   echo "screendump build/unsnapped.ppm"; sleep 1; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "snapped|is now" build/serial.log || echo "snap-test: nothing snapped"
	@grep -q "snapped Left" build/serial.log && grep -q "snapped Right" build/serial.log \
	    && echo "snap-test: dragged to the left half, then the right  PASS" \
	    || echo "snap-test: dragged to the left half, then the right  FAIL"
	@test "$$(grep -c 'unsnapped to 600x340' build/serial.log)" = 2 \
	    && grep "is now" build/serial.log | tail -1 | grep -q "600x340" \
	    && echo "snap-test: dragged away, back to its own size       PASS" \
	    || echo "snap-test: dragged away, back to its own size       FAIL"
	@test -s build/snap.ppm && pnmtopng build/snap.ppm > build/snap.png 2>/dev/null && rm -f build/snap.ppm && echo "wrote build/snap.png" || true
	@test -s build/snapped.ppm && pnmtopng build/snapped.ppm > build/snapped.png 2>/dev/null && rm -f build/snapped.ppm && echo "wrote build/snapped.png" || true
	@test -s build/unsnapped.ppm && pnmtopng build/unsnapped.ppm > build/unsnapped.png 2>/dev/null && rm -f build/unsnapped.ppm && echo "wrote build/unsnapped.png" || true

# Two notifications from the terminal, then Control Center from the clock:
# its Notifications card lists both, newest first (build/history.png).
history-test: iso
	@rm -f build/history.ppm build/history.png build/history-cleared.ppm build/history-cleared.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   for key in n o t i f y spc b u i l d spc d o n e ret; do echo "sendkey $$key"; sleep 0.1; done; sleep 0.5; \
	   for key in n o t i f y spc a l l spc t e s t s spc p a s s e d ret; do echo "sendkey $$key"; sleep 0.1; done; sleep 0.5; \
	   echo "mouse_move 280 -248"; sleep 0.2; echo "mouse_move 280 -248"; sleep 0.2; echo "mouse_move 280 -248"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "screendump build/history.ppm"; sleep 1; \
	   echo "mouse_move -24 178"; sleep 0.2; echo "mouse_move -24 178"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 0.8; \
	   echo "screendump build/history-cleared.ppm"; sleep 1; \
	   for key in right right ret; do echo "sendkey $$key"; sleep 0.4; done; sleep 0.8; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^notify |recent notifications" build/serial.log || echo "history-test: nothing"
	@grep -q "^control    : 2 recent notifications" build/serial.log \
	    && echo "history-test: Control Center listed both notifications PASS" \
	    || echo "history-test: Control Center listed both notifications FAIL"
	@grep -q "^control    : cleared the notifications" build/serial.log \
	    && grep -q "recent notifications cleared" build/serial.log \
	    && echo "history-test: Clear emptied the list                 PASS" \
	    || echo "history-test: Clear emptied the list                 FAIL"
	@grep -q "^control    : Glass turned Clear" build/serial.log \
	    && echo "history-test: the Glass tile went on to Clear        PASS" \
	    || echo "history-test: the Glass tile went on to Clear        FAIL"
	@test -s build/history.ppm && pnmtopng build/history.ppm > build/history.png 2>/dev/null && rm -f build/history.ppm && echo "wrote build/history.png" || true
	@test -s build/history-cleared.ppm && pnmtopng build/history-cleared.ppm > build/history-cleared.png 2>/dev/null && rm -f build/history-cleared.ppm && echo "wrote build/history-cleared.png" || true

# Control Center from the keyboard: the clock opens it, arrows walk to
# the 24-Hour tile (build/keys.png), Return flips it, Escape closes.
keys-test: iso
	@rm -f build/keys.ppm build/keys.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move 280 -193"; sleep 0.2; echo "mouse_move 280 -193"; sleep 0.4; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   for key in down down down right; do echo "sendkey $$key"; sleep 0.3; done; sleep 0.5; \
	   echo "screendump build/keys.ppm"; sleep 1; \
	   echo "sendkey ret"; sleep 0.8; echo "sendkey esc"; sleep 1; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^control " build/serial.log || echo "keys-test: no Control Center"
	@grep "^control    : keyboard on" build/serial.log | tail -1 | grep -q "24-Hour" \
	    && grep -q "^control    : 24-Hour turned" build/serial.log \
	    && grep -q "^control    : closed from the keyboard" build/serial.log \
	    && echo "keys-test: arrows, Return and Escape in Control Center PASS" \
	    || echo "keys-test: arrows, Return and Escape in Control Center FAIL"
	@test -s build/keys.ppm && pnmtopng build/keys.ppm > build/keys.png 2>/dev/null && rm -f build/keys.ppm && echo "wrote build/keys.png" || true

# Right-click the desktop: the menu opens at the pointer. Hover Next
# Wallpaper (build/menu.png, lit in the accent) and click it; then open
# the menu again and click outside it to close it (build/menu-after.png).
menu-test: iso
	@rm -f build/menu.ppm build/menu.png build/menu-after.ppm build/menu-after.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_button 2"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "mouse_move 40 80"; sleep 0.8; \
	   echo "screendump build/menu.ppm"; sleep 1; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "mouse_move -200 -100"; sleep 0.4; \
	   echo "mouse_button 2"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "mouse_move -150 0"; sleep 0.4; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.2; \
	   echo "screendump build/menu-after.ppm"; sleep 1; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^menu |right click|popover closed" build/serial.log || echo "menu-test: no menu"
	@test "$$(grep -c '^menu       : open' build/serial.log)" = 2 \
	    && grep -q "^menu       : picked Next Wallpaper" build/serial.log \
	    && grep -q "^menu       : closed" build/serial.log \
	    && echo "menu-test: right-click menu picked, then closed      PASS" \
	    || echo "menu-test: right-click menu picked, then closed      FAIL"
	@test -s build/menu.ppm && pnmtopng build/menu.ppm > build/menu.png 2>/dev/null && rm -f build/menu.ppm && echo "wrote build/menu.png" || true
	@test -s build/menu-after.ppm && pnmtopng build/menu-after.ppm > build/menu-after.png 2>/dev/null && rm -f build/menu-after.ppm && echo "wrote build/menu-after.png" || true

# Frame timing: drag a terminal around for a few seconds with glass
# Regular, then Clear, then Off (Control Center's Glass tile from the
# keyboard). build/serial.log has a "paint" line every two seconds of
# drawing; the "Glass turned" lines mark where each run starts.
DRAG_LOOP = for i in $$(seq 30); do echo "mouse_move 12 6"; sleep 0.04; done; \
	   for i in $$(seq 30); do echo "mouse_move -12 -6"; sleep 0.04; done
perf-test: iso
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 2; \
	   for step in 1 2 3; do echo "mouse_move -31 -218"; sleep 0.2; done; echo "mouse_move -1 -1"; sleep 0.5; \
	   for glass in regular clear off; do \
	     sleep 2.5; echo "mouse_button 1"; sleep 0.2; $(DRAG_LOOP); $(DRAG_LOOP); echo "mouse_button 0"; sleep 2.5; \
	     for step in 1 2 3 4; do echo "mouse_move 170 -22"; sleep 0.2; done; sleep 0.3; \
	     echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.2; \
	     for key in right right ret esc; do echo "sendkey $$key"; sleep 0.4; done; sleep 0.8; \
	     for step in 1 2 3 4; do echo "mouse_move -170 22"; sleep 0.2; done; \
	   done; echo quit ) \
	  | timeout --foreground 150 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^paint |Glass turned" build/serial.log || echo "perf-test: no frames"

# Settings, then a terminal that types "quit Settings": the terminal holds
# no shell endpoint, and the window server's refuses the call, so Settings
# stays (build/shell.png).
shell-test: iso
	@rm -f build/shell.ppm build/shell.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move 28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "mouse_move -56 0"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 2; \
	   for key in q u i t spc shift-s e t t i n g s ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 1.5; \
	   echo "screendump build/shell.ppm"; sleep 1; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "started|refused call|quit asked|\\$$ quit|window .* closed" build/serial.log || echo "shell-test: nothing"
	@grep -q "refused call 77 from task" build/serial.log \
	    && ! grep -q "quit asked" build/serial.log \
	    && ! grep -q "^settings   : closed" build/serial.log \
	    && echo "shell-test: the terminal could not quit Settings  PASS" \
	    || echo "shell-test: the terminal could not quit Settings  FAIL"
	@test -s build/shell.ppm && pnmtopng build/shell.ppm > build/shell.png 2>/dev/null && rm -f build/shell.ppm && echo "wrote build/shell.png" || true

# A terminal and Files from the dock, then Alt+D: both go
# (build/show-desktop.png); Alt+D again brings both back.
show-desktop-test: iso
	@rm -f build/show-desktop.ppm build/show-desktop.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "mouse_move -56 0"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "sendkey alt-d"; sleep 1.2; \
	   echo "screendump build/show-desktop.ppm"; sleep 1; \
	   echo "sendkey alt-d"; sleep 1.2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "ShowDesktop|desktop    : .*(minimized|showing|back from)" build/serial.log || echo "show-desktop-test: nothing"
	@grep -q "showing the desktop, 2 window" build/serial.log \
	    && grep -q "2 window(s) back from the desktop" build/serial.log \
	    && echo "show-desktop-test: Alt+D put both away and back  PASS" \
	    || echo "show-desktop-test: Alt+D put both away and back  FAIL"
	@test -s build/show-desktop.ppm && pnmtopng build/show-desktop.ppm > build/show-desktop.png 2>/dev/null && rm -f build/show-desktop.ppm && echo "wrote build/show-desktop.png" || true

# A terminal from the dock, then Alt+Left (build/tile.png), Alt+Right,
# Alt+Up and Alt+Down: left half, right half, the screen, and back.
tile-test: iso
	@rm -f build/tile.ppm build/tile.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 2; \
	   echo "sendkey alt-left"; sleep 1.2; \
	   echo "screendump build/tile.ppm"; sleep 1; \
	   for key in alt-right alt-up alt-down; do echo "sendkey $$key"; sleep 1.2; done; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "shortcut|snapped|zoomed|is now" build/serial.log || echo "tile-test: nothing"
	@grep -q "snapped Left" build/serial.log && grep -q "snapped Right" build/serial.log \
	    && grep -q "snapped Fill" build/serial.log \
	    && test "$$(grep 'is now' build/serial.log | tail -1 | grep -o '[0-9]*x[0-9]*')" = "600x340" \
	    && echo "tile-test: Alt+arrows snapped, Alt+Down put it back PASS" \
	    || echo "tile-test: Alt+arrows snapped, Alt+Down put it back FAIL"
	@test -s build/tile.ppm && pnmtopng build/tile.ppm > build/tile.png 2>/dev/null && rm -f build/tile.ppm && echo "wrote build/tile.png" || true

# A terminal from the dock; two slow clicks on its title bar do nothing,
# then a double click zooms it (build/double-click.png).
double-click-test: iso
	@rm -f build/double-click.ppm build/double-click.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 2; \
	   for step in 1 2 3; do echo "mouse_move -31 -218"; sleep 0.2; done; echo "mouse_move -1 -1"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.05; echo "mouse_button 0"; sleep 0.8; \
	   echo "mouse_button 1"; sleep 0.05; echo "mouse_button 0"; sleep 1; \
	   echo "mouse_button 1"; sleep 0.05; echo "mouse_button 0"; sleep 0.1; \
	   echo "mouse_button 1"; sleep 0.05; echo "mouse_button 0"; sleep 1.5; \
	   echo "screendump build/double-click.ppm"; sleep 1; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "on a title bar|double click|zoomed" build/serial.log || echo "double-click-test: nothing"
	@test "$$(grep -c 'double click on a title bar' build/serial.log)" -eq 1 \
	    && test "$$(grep -c 'desktop    : window .* zoomed' build/serial.log)" -eq 1 \
	    && echo "double-click-test: only the quick pair zoomed     PASS" \
	    || echo "double-click-test: only the quick pair zoomed     FAIL"
	@test -s build/double-click.ppm && pnmtopng build/double-click.ppm > build/double-click.png 2>/dev/null && rm -f build/double-click.ppm && echo "wrote build/double-click.png" || true

# A terminal from the dock, then cd /etc: its title bar becomes
# "Terminal - /etc" (build/title.png).
title-test: iso
	@rm -f build/title.ppm build/title.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 2; \
	   for key in c d spc slash e t c ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 1.5; \
	   echo "screendump build/title.ppm"; sleep 1; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "now says|^terminal   : \\$$ cd" build/serial.log || echo "title-test: nothing"
	@grep -q "now says Terminal - /[[:space:]]*$$" build/serial.log \
	    && grep -q "now says Terminal - /etc" build/serial.log \
	    && echo "title-test: the title bar follows the terminal's folder PASS" \
	    || echo "title-test: the title bar follows the terminal's folder FAIL"
	@test -s build/title.ppm && pnmtopng build/title.ppm > build/title.png 2>/dev/null && rm -f build/title.ppm && echo "wrote build/title.png" || true

# Two terminals, the first in /etc; the Terminal icon's menu lists both
# by folder (build/dock-windows.png), and picking /etc brings it forward.
dock-windows-test: iso
	@rm -f build/dock-windows.ppm build/dock-windows.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 2; \
	   for key in c d spc slash e t c ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 1; \
	   for step in 1 2 3; do echo "mouse_move -147 -248"; sleep 0.2; done; sleep 0.3; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   for step in 1 2 3; do echo "mouse_move 147 248"; sleep 0.2; done; sleep 0.5; \
	   echo "mouse_button 2"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "screendump build/dock-windows.ppm"; sleep 1; \
	   echo "mouse_move 0 -149"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^menu |now says|brought window" build/serial.log || echo "dock-windows-test: nothing"
	@grep -q "^menu       : picked /etc" build/serial.log \
	    && grep -q "brought window .* forward" build/serial.log \
	    && echo "dock-windows-test: the dock menu listed and raised a window PASS" \
	    || echo "dock-windows-test: the dock menu listed and raised a window FAIL"
	@test -s build/dock-windows.ppm && pnmtopng build/dock-windows.ppm > build/dock-windows.png 2>/dev/null && rm -f build/dock-windows.ppm && echo "wrote build/dock-windows.png" || true

# Two terminals from the dock, then a right click on its Terminal icon:
# the app's menu opens above it (build/dock-menu.png); Quit Terminal
# closes both windows.
dock-menu-test: iso
	@rm -f build/dock-menu.ppm build/dock-menu.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   for step in 1 2 3; do echo "mouse_move -147 -248"; sleep 0.2; done; sleep 0.3; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   for step in 1 2 3; do echo "mouse_move 147 248"; sleep 0.2; done; sleep 0.5; \
	   echo "mouse_button 2"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "screendump build/dock-menu.ppm"; sleep 1; \
	   echo "mouse_move 0 -53"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^menu |in the dock|quit asked|window .* closed|started Terminal" build/serial.log || echo "dock-menu-test: nothing"
	@grep -q "^menu       : picked Quit Terminal" build/serial.log \
	    && grep -q "quit asked 2 window" build/serial.log \
	    && echo "dock-menu-test: Quit from the dock closed every terminal PASS" \
	    || echo "dock-menu-test: Quit from the dock closed every terminal FAIL"
	@test -s build/dock-menu.ppm && pnmtopng build/dock-menu.ppm > build/dock-menu.png 2>/dev/null && rm -f build/dock-menu.ppm && echo "wrote build/dock-menu.png" || true

# Open a terminal from the dock and drag its bottom-right corner 200 by 100
# pixels bigger, in four steps; then drag its top-left corner 60 by 40
# bigger, the window moving to keep its bottom right where it is.
# build/resize.png shows the result.
resize-test: iso
	@rm -f build/resize.ppm build/resize.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "mouse_move 104 -304"; sleep 0.5; echo "mouse_button 1"; sleep 0.3; \
	   for step in 1 2 3 4; do echo "mouse_move 50 25"; sleep 0.3; done; \
	   echo "mouse_button 0"; sleep 1; \
	   echo "mouse_move -400 -230"; sleep 0.3; echo "mouse_move -394 -233"; sleep 0.5; echo "mouse_button 1"; sleep 0.3; \
	   for step in 1 2; do echo "mouse_move -30 -20"; sleep 0.3; done; \
	   echo "mouse_button 0"; sleep 1; \
	   echo "screendump build/resize.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "is now|^terminal   : resized" build/serial.log | tail -3 || true
	@grep -q "is now 800x440" build/serial.log \
	    && echo "resize-test: dragged the corner to 800x440            PASS" \
	    || echo "resize-test: dragged the corner to 800x440            FAIL"
	@grep -q "is now 860x480" build/serial.log \
	    && echo "resize-test: dragged the top left to 860x480          PASS" \
	    || echo "resize-test: dragged the top left to 860x480          FAIL"
	@test -s build/resize.ppm && pnmtopng build/resize.ppm > build/resize.png 2>/dev/null && rm -f build/resize.ppm && echo "wrote build/resize.png" || true

# Open a terminal and About from the dock, then work the windows from the
# keyboard: Alt+Tab to the terminal, Alt+M it away, Alt+comma for Settings,
# Alt+Q to close Settings, Alt+Tab to bring the terminal back.
shortcut-test: iso
	@rm -f build/shortcut.ppm build/shortcut.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "mouse_move 112 0"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   for key in alt-tab alt-m alt-comma; do echo "sendkey $$key"; sleep 1.5; done; \
	   echo "sendkey alt-q"; sleep 1.5; echo "sendkey alt-tab"; sleep 1.5; \
	   echo "screendump build/shortcut.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^(input|dock|desktop|settings) " build/serial.log | grep -v "PASS\|pointer moved" || echo "shortcut-test: nothing from the desktop"
	@grep -q "minimized" build/serial.log && grep -q "started Settings" build/serial.log \
	    && grep -q "^settings   : closed" build/serial.log && grep -q "restored" build/serial.log \
	    && echo "shortcut-test: switch, minimize, settings, close        PASS" \
	    || echo "shortcut-test: switch, minimize, settings, close        FAIL"
	@test -s build/shortcut.ppm && pnmtopng build/shortcut.ppm > build/shortcut.png 2>/dev/null && rm -f build/shortcut.ppm && echo "wrote build/shortcut.png" || true

# Open a terminal from the dock and type exit in it, twenty times: more
# windows than there are surfaces, so it passes only if a closed window's
# surface goes to the next one.
churn-test: iso
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   for round in $$(seq 20); do \
	       echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.2; \
	       for key in e x i t ret; do echo "sendkey $$key"; sleep 0.1; done; sleep 0.8; \
	   done; \
	   echo "screendump build/churn.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 150 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@echo "churn-test: $$(grep -c 'dock       : started Terminal' build/serial.log) started, $$(grep -c 'desktop    : window .* closed' build/serial.log) closed"
	@test "$$(grep -c 'desktop    : window .* closed' build/serial.log)" = 20 \
	    && echo "churn-test: twenty windows opened and closed           PASS" \
	    || echo "churn-test: twenty windows opened and closed           FAIL"
	@test -s build/churn.ppm && pnmtopng build/churn.ppm > build/churn.png 2>/dev/null && rm -f build/churn.ppm && echo "wrote build/churn.png" || true

# Open Settings from the dock and change the look three times while it runs:
# Light, the Dunes wallpaper, a blue accent. build/settings.png shows the
# result.
settings-test: iso
	@rm -f build/settings.ppm build/settings.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move 28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   echo "mouse_move -80 -515"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   echo "mouse_move -52 114"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   echo "mouse_move -218 -76"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   echo "mouse_move 406 477"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   echo "screendump build/settings.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^(input|dock|desktop|settings|theme) " build/serial.log | grep -v PASS || echo "settings-test: nothing from the desktop"
	@test "$$(grep -c '^theme      : changed' build/serial.log)" = 3 \
	    && echo "settings-test: the look changed three times           PASS" \
	    || echo "settings-test: the look changed three times           FAIL"
	@test -s build/settings.ppm && pnmtopng build/settings.ppm > build/settings.png 2>/dev/null && rm -f build/settings.ppm && echo "wrote build/settings.png" || true

# Start a terminal from the dock, minimize it with its yellow button, bring
# it back from the dock, then zoom it with its green one. build/minimize.png
# shows it centred.
minimize-test: iso
	@rm -f build/minimize.ppm build/minimize.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   echo "mouse_move -452 -654"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 1; \
	   echo "mouse_move 452 654"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 1; \
	   echo "mouse_move -432 -654"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 1.5; \
	   echo "mouse_move 300 200"; sleep 0.5; \
	   echo "screendump build/minimize.ppm"; sleep 2; \
	   echo "mouse_move -412 -250"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 1.5; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^(input|dock|desktop) " build/serial.log | grep -v PASS || echo "minimize-test: nothing from the desktop"
	@grep -q "minimized" build/serial.log && grep -q "restored" build/serial.log && grep -q "zoomed" build/serial.log \
	    && grep -q "is now 1264x640" build/serial.log && grep -q "is now 600x340" build/serial.log \
	    && echo "minimize-test: minimized, restored and zoomed          PASS" \
	    || echo "minimize-test: minimized, restored and zoomed          FAIL"
	@test -s build/minimize.ppm && pnmtopng build/minimize.ppm > build/minimize.png 2>/dev/null && rm -f build/minimize.ppm && echo "wrote build/minimize.png" || true

# Boot, start a terminal from the dock's first icon, then click the second.
# build/dock.png shows the dock with the terminal's running dot under it.
dock-test: iso
	@rm -f build/dock.ppm build/dock.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   echo "mouse_move 56 0"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 0.3; \
	   echo "screendump build/dock.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^(input|dock|terminal|desktop) " build/serial.log | grep -v PASS || echo "dock-test: nothing from the desktop"
	@grep -q "dock       : started Terminal" build/serial.log \
	    && echo "dock-test: the dock started a terminal                    PASS" \
	    || echo "dock-test: the dock started a terminal                    FAIL"
	@test -s build/dock.ppm && pnmtopng build/dock.ppm > build/dock.png 2>/dev/null && rm -f build/dock.ppm && echo "wrote build/dock.png" || true

# Boot and rest the pointer on the dock's Terminal icon: it grows, its
# neighbours a little, the dock widens, and a click on it still starts a
# terminal. build/magnify.png shows it grown.
magnify-test: iso
	@rm -f build/magnify.ppm build/magnify.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 1.2; \
	   echo "screendump build/magnify.ppm"; sleep 1; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^dock " build/serial.log | grep -v PASS || echo "magnify-test: nothing from the dock"
	@grep -q "dock       : Terminal grows to 67 px" build/serial.log \
	    && echo "magnify-test: the icon under the pointer grows            PASS" \
	    || echo "magnify-test: the icon under the pointer grows            FAIL"
	@grep -q "dock       : started Terminal" build/serial.log \
	    && echo "magnify-test: and a click on it still starts it           PASS" \
	    || echo "magnify-test: and a click on it still starts it           FAIL"
	@test -s build/magnify.ppm && pnmtopng build/magnify.ppm > build/magnify.png 2>/dev/null && rm -f build/magnify.ppm && echo "wrote build/magnify.png" || true

# Open Settings from the dock, set Magnify to Large, then rest on the dock's
# Terminal icon: it grows to 170% now, 81 px. build/magnify-large.png.
magnify-setting-test: iso
	@rm -f build/magnify-large.ppm build/magnify-large.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move 28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   echo "mouse_move -29 -250"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   echo "mouse_move -27 250"; sleep 1.2; \
	   echo "screendump build/magnify-large.ppm"; sleep 1; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^(dock|theme) " build/serial.log | grep -v PASS || echo "magnify-setting-test: nothing from the desktop"
	@grep -q "^theme      : changed" build/serial.log \
	    && echo "magnify-setting-test: Settings changed the look         PASS" \
	    || echo "magnify-setting-test: Settings changed the look         FAIL"
	@grep -q "dock       : Terminal grows to 81 px" build/serial.log \
	    && echo "magnify-setting-test: and the dock now grows to 170%     PASS" \
	    || echo "magnify-setting-test: and the dock now grows to 170%     FAIL"
	@test -s build/magnify-large.ppm && pnmtopng build/magnify-large.ppm > build/magnify-large.png 2>/dev/null && rm -f build/magnify-large.ppm && echo "wrote build/magnify-large.png" || true

# Boot, open two terminals from the panel's button, close the one on top
# with its title bar's close button and the other by typing exit. Both
# windows should go; build/close.png shows the desktop they leave.
close-test: iso
	@rm -f build/close.ppm build/close.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -230 -190"; sleep 0.2; echo "mouse_move -230 -190"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   echo "mouse_move 0 120"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 1.5; \
	   echo "mouse_move 220 160"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 0.5; \
	   for key in e x i t ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 1.5; \
	   echo "screendump build/close.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^(input|panel|terminal|desktop) " build/serial.log || echo "close-test: nothing from the desktop"
	@test "$$(grep -c 'window .* closed' build/serial.log)" = 2 \
	    && echo "close-test: both windows closed                             PASS" \
	    || echo "close-test: both windows closed                             FAIL"
	@test -s build/close.ppm && pnmtopng build/close.ppm > build/close.png 2>/dev/null && rm -f build/close.ppm && echo "wrote build/close.png" || true

# Boot, open the terminal from the panel's button (the moves add up to the
# button from where the pointer starts), and use its prompt: Tab to
# complete, arrows to edit mid-line and to recall history, Ctrl+U to throw a
# line away, and a pipe that reads one of the files from root/.
# build/terminal.png shows what it made of it.
TERMINAL_KEYS ?= e c tab h i ret \
                 u n a m e spc minus a ret \
                 u tab ctrl-u \
                 up up ret \
                 e c o spc w o r l d home right right h ret \
                 h i s tab ret \
                 f r e e ret \
                 c a t spc slash e t c slash m o t d spc shift-backslash spc w c spc minus w ret
terminal-test: iso
	@rm -f build/terminal.ppm build/terminal.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move 120 -80"; sleep 0.2; \
	   echo "mouse_move -250 -150"; sleep 0.2; echo "mouse_move -250 -150"; sleep 0.2; \
	   echo "mouse_move -80 0"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   for key in $(TERMINAL_KEYS); do echo "sendkey $$key"; sleep 0.15; done; sleep 1; \
	   echo "screendump build/terminal.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^terminal " build/serial.log || echo "terminal-test: the terminal said nothing"
	@test -s build/terminal.ppm && pnmtopng build/terminal.ppm > build/terminal.png 2>/dev/null && rm -f build/terminal.ppm && echo "wrote build/terminal.png" || true

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
