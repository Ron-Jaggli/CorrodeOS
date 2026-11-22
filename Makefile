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
# A raw disk image to keep settings on between boots, attached as a virtio
# disk; none by default, so no test's theme leaks into the next one's.
# `run` and `run-gui` keep theirs in build/settings.img.
STORE ?=
SETTINGS_IMG := build/settings.img
store_flags = -drive file=$(1),format=raw,if=none,id=store \
              -device virtio-blk-pci,drive=store,disable-legacy=on

# USB=1: an xHCI controller with QEMU's USB keyboard and tablet on it,
# emulated: nothing here may name a host device. QEMU hands keys to the USB
# keyboard once it exists, and a tablet takes positions, which the PS/2
# mouse cannot: what reaches the desktop then came through the USB driver.
USB ?=
usb_flags = -device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 -device usb-tablet,bus=xhci.0

QEMU_BASE := -M $(MACHINE) -m $(MEM) -smp $(CPUS) \
             -cdrom $(ISO) -boot d \
             -net none \
             -no-reboot -no-shutdown \
             -vga $(GPU) $(ACCEL) $(QEMU_DEVICES) $(QEMU_EXTRA) \
             $(if $(STORE),$(call store_flags,$(STORE))) \
             $(if $(USB),$(usb_flags))

QEMU_COMMON := $(QEMU_BASE) -display none
QEMU_FLAGS  := $(QEMU_COMMON) -serial stdio

# VNC display number; :0 is TCP 5900, which is what ./corrode publishes.
VNC_DISPLAY ?= 0


.PHONY: input-test terminal-test close-test dock-test minimize-test settings-test churn-test shortcut-test resize-test files-test scrollback-test switcher-test cursor-test search-test reveal-test notify-test control-test lock-test snap-test history-test keys-test menu-test dock-menu-test title-test dock-windows-test double-click-test tile-test show-desktop-test shell-test magnify-test magnify-setting-test store-test passcode-test passcode-hash seal-test surfaces-test fade-test usb-test pe-test verify-test vault-test rekey-test win-test files-run-test grow-test zoom-test
.PHONY: all kernel user kernel32 iso vault-iso vault-disk run-vault run run-gui boot-test fault-test wx-test stack-test user-test screenshot specimen engine-shot check test fmt clippy clean-artifacts

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
USER_PROGRAMS := edu fault virtio-gpu virtio-blk blob-reader nvidia ps2 xhci terminal panel settings about files control lock menu fileview
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
	cargo test -p corrode-render -p corrode-text -p corrode-gfx -p corrode-pci -p corrode-elf -p corrode-pe -p corrode-virtio -p corrode-nv -p corrode-input -p corrode-heap -p corrode-line -p corrode-shell -p corrode-theme -p corrode-seal -p corrode-vault -p corrode-view --target $(HOST_TARGET)

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
READER_EXE := build/reader.exe
SPIN_EXE := build/spin.exe
DIR_EXE := build/dir.exe
FAULT_EXE := build/fault.exe
HELLO_C_EXE := build/hello-c.exe
ROOT_FILES := $(shell find root 2>/dev/null)
# With reader.exe at /programs/reader, spin.exe (which never ends) at
# /programs/spin, fault.exe (which faults) at /programs/fault and dir.exe
# (compiled, not assembled) at /programs/dir, for the terminal's `win`:
# after
# every other top folder, so none of them moves in Files.
$(ROOT_TAR): $(ROOT_FILES) $(READER_EXE) $(SPIN_EXE) $(DIR_EXE) $(FAULT_EXE) $(HELLO_C_EXE)
	@mkdir -p build
	@rm -rf build/root_staged && cp -r root build/root_staged
	@mkdir -p build/root_staged/programs/reader && cp $(READER_EXE) build/root_staged/programs/reader/reader.exe
	@mkdir -p build/root_staged/programs/spin && cp $(SPIN_EXE) build/root_staged/programs/spin/spin.exe
	@mkdir -p build/root_staged/programs/fault && cp $(FAULT_EXE) build/root_staged/programs/fault/fault.exe
	@mkdir -p build/root_staged/programs/dir && cp $(DIR_EXE) build/root_staged/programs/dir/dir.exe
	@mkdir -p build/root_staged/programs/c && cp $(HELLO_C_EXE) build/root_staged/programs/c/hello.exe
	tar --format=ustar --sort=name --owner=0 --group=0 --numeric-owner \
	    --mtime=@0 --mode=u+rw,go+r,go-w -C build/root_staged -cf $@ .

# Verdigris's first Windows program, a PE file written byte by byte by a
# script: no Windows toolchain. See verdigris/DESIGN.md.
HELLO_EXE := build/hello.exe
$(HELLO_EXE): verdigris/scripts/hello-exe.sh
	@mkdir -p build
	bash verdigris/scripts/hello-exe.sh $@

# Phase 2: a console program that calls kernel32, and Verdigris's kernel32,
# linked at its own address by its own script -- so built apart from the
# user programs, which share user/user.ld.
CONSOLE_EXE := build/console.exe
$(CONSOLE_EXE): verdigris/scripts/console-exe.sh
	@mkdir -p build
	bash verdigris/scripts/console-exe.sh $@

# Phase 3: a program that reads files through its view, assembled whole --
# headers and all -- by GNU as.
$(READER_EXE): verdigris/scripts/reader-exe.sh verdigris/scripts/reader-exe.S
	@mkdir -p build
	bash verdigris/scripts/reader-exe.sh $@

$(DIR_EXE): verdigris/scripts/dir-exe.sh verdigris/dir/src/main.rs
	@mkdir -p build
	bash verdigris/scripts/dir-exe.sh $@

# A C program compiled the ordinary way, by mingw-w64's gcc with no flags of
# Verdigris's: it imports msvcrt.dll and kernel32.dll and starts through
# mingw's own start-up code. A test program only; Verdigris is Rust.
$(HELLO_C_EXE): verdigris/c/hello.c
	@mkdir -p build
	x86_64-w64-mingw32-gcc -O2 -s -o $@ $<

$(SPIN_EXE): verdigris/scripts/reader-exe.sh verdigris/scripts/spin-exe.S
	@mkdir -p build
	bash verdigris/scripts/reader-exe.sh $@ spin-exe.S

$(FAULT_EXE): verdigris/scripts/reader-exe.sh verdigris/scripts/fault-exe.S
	@mkdir -p build
	bash verdigris/scripts/reader-exe.sh $@ fault-exe.S

KERNEL32_TARGET := target/verdigris
KERNEL32 := $(KERNEL32_TARGET)/x86_64-unknown-none/release/kernel32
KERNEL32_RUSTFLAGS := -C relocation-model=static -C strip=debuginfo \
                      -C link-arg=-T$(CURDIR)/verdigris/kernel32/kernel32.ld

kernel32:
	RUSTFLAGS="$(KERNEL32_RUSTFLAGS)" cargo build --release --target-dir $(KERNEL32_TARGET) \
	    -p corrode-kernel32

iso: $(KERNEL) user kernel32 $(TEST_BLOB) $(ROOT_TAR) $(HELLO_EXE) $(CONSOLE_EXE) $(READER_EXE)
	@rm -rf $(ISO_ROOT)/boot/programs $(ISO_ROOT)/boot/firmware $(ISO_ROOT)/boot/files
	@mkdir -p $(ISO_ROOT)/boot/limine $(ISO_ROOT)/boot/programs $(ISO_ROOT)/boot/firmware $(ISO_ROOT)/boot/files $(ISO_ROOT)/EFI/BOOT
	cp $(KERNEL) $(ISO_ROOT)/boot/corrode
	cp $(foreach program,$(USER_PROGRAMS),$(USER_BIN)/$(program)) $(ISO_ROOT)/boot/programs/
	cp $(HELLO_EXE) $(ISO_ROOT)/boot/programs/hello.exe
	cp $(CONSOLE_EXE) $(ISO_ROOT)/boot/programs/console.exe
	cp $(READER_EXE) $(ISO_ROOT)/boot/programs/reader.exe
	cp $(KERNEL32) $(ISO_ROOT)/boot/programs/kernel32
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
	   echo "    module_path: boot():/boot/programs/hello.exe"; \
	   echo "    module_path: boot():/boot/programs/console.exe"; \
	   echo "    module_path: boot():/boot/programs/reader.exe"; \
	   echo "    module_path: boot():/boot/programs/kernel32"; \
	   echo "    module_path: boot():/boot/firmware/test.blob"; \
	   echo "    module_path: boot():/boot/files/root.tar"; \
	   for file in $(FIRMWARE); do \
	       echo "    module_path: boot():/boot/$$file"; \
	   done; } > $(ISO_ROOT)/boot/limine/limine.conf
	@# Hashes on every path, the config's hash enrolled into Limine.
	LIMINE_DIR=$(LIMINE_DIR) bash scripts/seal-iso.sh $(ISO_ROOT) $(ISO)
	@echo "built $(ISO)"

# Limine boots only what was built (scripts/seal-iso.sh). Copies of the
# ISO's tree, each packed as it is, not sealed again: the copy untouched,
# which must boot (so packing is not what stops the others); a byte of the
# kernel changed, and a byte of a program, which Limine must refuse by
# name; and the config with one hash taken off, which must stop Limine
# before it reads a line of it -- the config's own hash, enrolled, no
# longer matches.
VERIFY_TIMEOUT ?= 8
verify-test: iso
	@rm -rf build/verify_root build/verify.iso build/verify-*.log
	@for case in none kernel program config; do \
	    rm -rf build/verify_root && cp -r $(ISO_ROOT) build/verify_root; \
	    case $$case in \
	    kernel)  printf '\x90' | dd of=build/verify_root/boot/corrode bs=1 seek=4096 conv=notrunc status=none ;; \
	    program) printf '\x90' | dd of=build/verify_root/boot/programs/terminal bs=1 seek=4096 conv=notrunc status=none ;; \
	    config)  sed -i '0,/#[0-9a-f]*$$/s/#[0-9a-f]*$$//' build/verify_root/boot/limine/limine.conf ;; \
	    esac; \
	    LIMINE_DIR=$(LIMINE_DIR) bash scripts/seal-iso.sh --pack-only build/verify_root build/verify.iso; \
	    timeout --foreground $(VERIFY_TIMEOUT) qemu-system-x86_64 \
	        $(subst $(ISO),build/verify.iso,$(QEMU_FLAGS)) < /dev/null > build/verify-$$case.log 2>&1 || true; \
	done
	@rm -rf build/verify_root build/verify.iso
	@grep -aq "^Corrode v" build/verify-none.log \
	    && echo "verify-test: the tree as built boots             PASS" \
	    || echo "verify-test: the tree as built boots             FAIL"
	@grep -aq "hash for URI .boot():/boot/corrode. does not match" build/verify-kernel.log \
	    && ! grep -aq "^Corrode v" build/verify-kernel.log \
	    && echo "verify-test: a changed kernel is refused         PASS" \
	    || echo "verify-test: a changed kernel is refused         FAIL"
	@grep -aq "hash for URI .boot():/boot/programs/terminal. does not match" build/verify-program.log \
	    && ! grep -aq "^Corrode v" build/verify-program.log \
	    && echo "verify-test: a changed program is refused        PASS" \
	    || echo "verify-test: a changed program is refused        FAIL"
	@! grep -aq "limine: Loading\|^Corrode v" build/verify-config.log \
	    && echo "verify-test: an edited config is refused         PASS" \
	    || echo "verify-test: an edited config is refused         FAIL"

# Interactive boot. Ctrl-A X to quit QEMU.
run: iso
	@test -f $(SETTINGS_IMG) || truncate -s 1M $(SETTINGS_IMG)
	qemu-system-x86_64 $(QEMU_FLAGS) $(if $(STORE),,$(call store_flags,$(SETTINGS_IMG)))

# Live framebuffer over VNC, serial still on stdout.
#
# QEMU binds the VNC socket on all interfaces *inside the container*, because
# podman's published-port forwarding arrives on the container's veth, not its
# loopback. Exposure is clamped on the host side instead: ./corrode publishes it
# to 127.0.0.1 only, so nothing outside this machine can reach it.
run-gui: iso
	@echo "VNC listening -- connect to 127.0.0.1:590$(VNC_DISPLAY)"
	@test -f $(SETTINGS_IMG) || truncate -s 1M $(SETTINGS_IMG)
	qemu-system-x86_64 $(QEMU_BASE) -vnc :$(VNC_DISPLAY) -serial stdio \
	    $(if $(STORE),,$(call store_flags,$(SETTINGS_IMG)))

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

# Verdigris, phases 1 and 2: boot and check hello.exe loaded, mapped as
# its sections ask, said its line through the log, and exited, and that a
# damaged copy was refused; then that console.exe's imports were bound to
# kernel32 and its WriteFile reached the log.
pe-test: iso
	@timeout --foreground $(BOOT_TIMEOUT) \
	    qemu-system-x86_64 $(QEMU_FLAGS) < /dev/null > build/serial.log 2>&1 || true
	@grep -a "^verdigris \|^hello.exe \|^console.exe\|^reader.exe \|^fileview " build/serial.log || echo "pe-test: nothing from Verdigris"
	@grep -aq "^hello.exe  : hello from a PE, over int 0x80" build/serial.log \
	    && echo "pe-test: a Windows program said its line             PASS" \
	    || echo "pe-test: a Windows program said its line             FAIL"
	@grep -aq "^console.exe: hello from kernel32, via WriteFile" build/serial.log \
	    && echo "pe-test: ...and one through kernel32's WriteFile     PASS" \
	    || echo "pe-test: ...and one through kernel32's WriteFile     FAIL"
	@test $$(grep -a "^verdigris " build/serial.log | grep -ac "PASS") -eq 13 \
	    && ! grep -a "^verdigris " build/serial.log | grep -aq "FAIL" \
	    && echo "pe-test: the kernel's 13 Verdigris checks pass       PASS" \
	    || echo "pe-test: the kernel's 13 Verdigris checks pass       FAIL"
	@# Phase 3: reader.exe, in /docs, allowed /etc/motd besides. Whole
	@# lines are matched, so without the serial port's carriage returns.
	@tr -d '\r' < build/serial.log > build/pe-lines.log
	@motd=$$(head -1 root/etc/motd); keys=$$(head -1 root/docs/keys.txt); \
	grep -aqxF "reader.exe : keys.txt: $$keys" build/pe-lines.log \
	    && grep -aqxF "reader.exe : c:/Etc/MOTD: $$motd" build/pe-lines.log \
	    && grep -aqxF "reader.exe : ..\..\etc\motd: $$motd" build/pe-lines.log \
	    && test $$(grep -acxF "reader.exe : GetFileSize and the next ReadFile agree" build/pe-lines.log) -eq 4 \
	    && echo "pe-test: reader.exe reads what it may, any spelling  PASS" \
	    || echo "pe-test: reader.exe reads what it may, any spelling  FAIL"
	@grep -aqxF 'reader.exe : C:\etc\desktop.conf: not there (2, ERROR_FILE_NOT_FOUND)' build/pe-lines.log \
	    && grep -aqxF 'reader.exe : C:\etc\nothing.txt: not there (2, ERROR_FILE_NOT_FOUND)' build/pe-lines.log \
	    && grep -aqxF 'reader.exe : C:\home\hello.txt: not there (3, ERROR_PATH_NOT_FOUND)' build/pe-lines.log \
	    && grep -aqxF 'reader.exe : D:\etc\motd: not there (3, ERROR_PATH_NOT_FOUND)' build/pe-lines.log \
	    && echo "pe-test: ...the rest is absent, as a missing name is PASS" \
	    || echo "pe-test: ...the rest is absent, as a missing name is FAIL"
	@grep -aqxF 'reader.exe : C:\etc\motd, to write: read-only (19, ERROR_WRITE_PROTECT)' build/pe-lines.log \
	    && echo "pe-test: ...and nothing it sees can be written       PASS" \
	    || echo "pe-test: ...and nothing it sees can be written       FAIL"
	@# Listings: each search's names on one line, then why it ended.
	@listed() { grep -aF "reader.exe : $$1: " build/pe-lines.log | sed 's/.*: //' | tr '\n' ' '; }; \
	end='no more (18, ERROR_NO_MORE_FILES) '; \
	test "$$(listed 'C:\*')" = "docs\ etc\ $$end" \
	    && test "$$(listed 'C:\etc\*')" = ".\ ..\ motd $$end" \
	    && test "$$(listed 'c:/docs/*.TXT')" = "$$(cd root/docs && LC_ALL=C ls *.txt | tr '\n' ' ')$$end" \
	    && test "$$(listed 'C:\etc\m?td')" = "motd $$end" \
	    && echo "pe-test: a search shows only what the view shows    PASS" \
	    || echo "pe-test: a search shows only what the view shows    FAIL"
	@grep -aqxF 'reader.exe : C:\etc\desktop.c*: not there (2, ERROR_FILE_NOT_FOUND)' build/pe-lines.log \
	    && grep -aqxF 'reader.exe : C:\home\*: not there (3, ERROR_PATH_NOT_FOUND)' build/pe-lines.log \
	    && echo "pe-test: ...and finds a hidden name as a missing one PASS" \
	    || echo "pe-test: ...and finds a hidden name as a missing one FAIL"
	@# The current directory: moved, followed by a relative open, shown and
	@# made full only as C:\ paths.
	@motd=$$(head -1 root/etc/motd); \
	grep -aqxF 'reader.exe : current directory: C:\docs' build/pe-lines.log \
	    && grep -aqxF 'reader.exe : SetCurrentDirectoryW(..\etc): done' build/pe-lines.log \
	    && grep -aqxF 'reader.exe : current directory: C:\etc' build/pe-lines.log \
	    && grep -aqxF "reader.exe : motd: $$motd" build/pe-lines.log \
	    && grep -aqxF 'reader.exe : GetFullPathNameW(..\home\hello.txt): C:\home\hello.txt, file part hello.txt' build/pe-lines.log \
	    && grep -aqxF 'reader.exe : GetFullPathNameW(..\..\..\x): C:\x, file part x' build/pe-lines.log \
	    && grep -aqxF 'reader.exe : SetCurrentDirectoryW(C:\home): not there (2, ERROR_FILE_NOT_FOUND)' build/pe-lines.log \
	    && grep -aqxF 'reader.exe : SetCurrentDirectoryW(C:\etc\motd): not a directory (267, ERROR_DIRECTORY)' build/pe-lines.log \
	    && grep -aqxF 'reader.exe : current directory: C:\' build/pe-lines.log \
	    && echo "pe-test: the current directory moves inside the view PASS" \
	    || echo "pe-test: the current directory moves inside the view FAIL"
	@# Every path it was given -- current directories, full paths -- is a
	@# C:\ path; a real one would start with `/`.
	@given=$$(grep -aE '^reader.exe : (current directory|GetFullPathNameW\([^)]*\)): ' build/pe-lines.log \
	          | sed 's/^[^:]*: [^:]*: //'); \
	test $$(printf '%s\n' "$$given" | grep -c .) -eq 5 \
	    && ! printf '%s\n' "$$given" | grep -vq '^C:\\[^/]*$$' \
	    && echo "pe-test: ...and every path it is given is a C:\ one PASS" \
	    || echo "pe-test: ...and every path it is given is a C:\ one FAIL"

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

# Open Files from the dock and run a Windows program from it: down four to
# /programs, in, down to dir, in, down to dir.exe, Return. Its output is
# in the preview (build/files-run.png) and the log; it saw its folder
# alone, as for the terminal's `win`.
files-run-test: iso
	@rm -f build/files-run.ppm build/files-run.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -84 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 2; \
	   for key in down down down down ret down down ret down ret; do echo "sendkey $$key"; sleep 0.6; done; sleep 3; \
	   echo "screendump build/files-run.ppm"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@tr -d '\r' < build/serial.log > build/files-run-lines.log
	@grep -a "^files      : dir.exe\|^desktop    : /programs/dir" build/files-run-lines.log || echo "files-run-test: nothing from dir.exe"
	@size=$$(stat -c %s $(DIR_EXE)) \
	    && grep -aqxF 'desktop    : /programs/dir/dir.exe started for Files, seeing /programs/dir alone' build/files-run-lines.log \
	    && grep -aqxF "files      : dir.exe: dir.exe  $$size" build/files-run-lines.log \
	    && grep -aqxF "files      : dir.exe: 1 files, $$size bytes" build/files-run-lines.log \
	    && grep -aqxF 'files      : dir.exe: ended after 2 lines' build/files-run-lines.log \
	    && grep -aqE '^verdigris  : moved from 0x140000000 to 0x400000, [1-9][0-9]* relocations$$' build/files-run-lines.log \
	    && echo "files-run-test: Return on dir.exe ran it, its folder alone PASS" \
	    || echo "files-run-test: Return on dir.exe ran it, its folder alone FAIL"
	@test -s build/files-run.ppm && pnmtopng build/files-run.ppm > build/files-run.png 2>/dev/null && rm -f build/files-run.ppm && echo "wrote build/files-run.png" || true

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
	    && grep -q "^control    : the card resizes for 0 notification(s)" build/serial.log \
	    && echo "history-test: Clear emptied the list                 PASS" \
	    || echo "history-test: Clear emptied the list                 FAIL"
	@grep -q "^control    : Glass turned Clear" build/serial.log \
	    && echo "history-test: the Glass tile went on to Clear        PASS" \
	    || echo "history-test: the Glass tile went on to Clear        FAIL"
	@test -s build/history.ppm && pnmtopng build/history.ppm > build/history.png 2>/dev/null && rm -f build/history.ppm && echo "wrote build/history.png" || true
	@test -s build/history-cleared.ppm && pnmtopng build/history-cleared.ppm > build/history-cleared.png 2>/dev/null && rm -f build/history-cleared.ppm && echo "wrote build/history-cleared.png" || true

# A window zooms in from 90% as it opens: Terminal from the dock, a
# screenshot a moment in (build/zoom-in.png) and once it has settled
# (build/zoom.png).
zoom-test: iso
	@rm -f build/zoom-in.ppm build/zoom-in.png build/zoom.ppm build/zoom.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.05; echo "mouse_button 0"; sleep 0.02; \
	   echo "screendump build/zoom-in.ppm"; sleep 1.5; \
	   echo "screendump build/zoom.ppm"; sleep 0.5; echo quit ) \
	  | timeout --foreground 60 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "zooms in" build/serial.log || echo "zoom-test: no window opened"
	@grep -q "^desktop    : window [0-9]* zooms in from 90%" build/serial.log \
	    && echo "zoom-test: the terminal zoomed in as it opened        PASS" \
	    || echo "zoom-test: the terminal zoomed in as it opened        FAIL"
	@test -s build/zoom-in.ppm && pnmtopng build/zoom-in.ppm > build/zoom-in.png 2>/dev/null && rm -f build/zoom-in.ppm || true
	@test -s build/zoom.ppm && pnmtopng build/zoom.ppm > build/zoom.png 2>/dev/null && rm -f build/zoom.ppm && echo "wrote build/zoom.png" || true

# A window fading shows no line under its title bar. The frame behind a
# titled window is the bar and one corner's radius more, reaching under
# the top of the body; a body half faded would show that edge through it.
# A copy of the ISO with animations at a twentieth of their speed
# (build/fade.iso), so the fades are seconds long: a terminal from the
# dock, a screenshot as it zooms in (build/fade-open.png, by eye), and
# once it has settled one as Alt+Q fades it out (build/fade.png), where
# it is still: the first window is at (120, 90). scripts/rim.sh measures
# the steepest step down the body's margin under the bar; an edge there
# is tens of levels, smooth is a few.
FADE_ISO := build/fade.iso
RIM_MOST ?= 6
fade-test: iso
	@rm -rf build/fade_root build/fade_iso_root $(FADE_ISO) build/fade*.ppm build/fade*.png
	@cp -r root build/fade_root && printf '\nanimation_speed = 5\n' >> build/fade_root/etc/desktop.conf
	@cp -r $(ISO_ROOT) build/fade_iso_root
	@tar --format=ustar --sort=name --owner=0 --group=0 --numeric-owner \
	    --mtime=@0 --mode=u+rw,go+r,go-w -C build/fade_root -cf build/fade_iso_root/boot/files/root.tar .
	@LIMINE_DIR=$(LIMINE_DIR) bash scripts/seal-iso.sh build/fade_iso_root $(FADE_ISO)
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 0.35; \
	   echo "screendump build/fade-open.ppm"; sleep 8; \
	   echo "screendump build/fade-settled.ppm"; sleep 1; \
	   echo "sendkey alt-q"; sleep 0.8; \
	   echo "screendump build/fade.ppm"; sleep 5; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(subst $(ISO),$(FADE_ISO),$(QEMU_COMMON)) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -q "^theme      : read /etc/desktop.conf" build/serial.log || echo "fade-test: the slowed desktop.conf was not read"
	@echo "fade-test: steepest step settled $$(scripts/rim.sh build/fade-settled.ppm 120 90), fading $$(scripts/rim.sh build/fade.ppm 120 90) (most $(RIM_MOST))"
	@grep -aq "desktop    : window .* closed" build/serial.log \
	    && [ "$$(scripts/rim.sh build/fade.ppm 120 90 | cut -d' ' -f1 | cut -d. -f1)" -le $(RIM_MOST) ] \
	    && [ "$$(scripts/rim.sh build/fade-settled.ppm 120 90 | cut -d' ' -f1 | cut -d. -f1)" -le $(RIM_MOST) ] \
	    && echo "fade-test: no rim under the bar of a fading window    PASS" \
	    || echo "fade-test: no rim under the bar of a fading window    FAIL"
	@for shot in fade-open fade-settled fade; do test -s build/$$shot.ppm && pnmtopng build/$$shot.ppm > build/$$shot.png 2>/dev/null && rm -f build/$$shot.ppm; done; echo "wrote build/fade.png"

# Control Center's notifications card grows while it is open. A terminal
# sleeps, with two notifications queued behind the sleep; Control Center
# opens with none; they arrive, Control Center is told at once, and the
# card grows to show them well before its own 20 s look.
# build/grow-before.png and build/grow.png.
grow-test: iso
	@rm -f build/grow-before.ppm build/grow-before.png build/grow.ppm build/grow.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   for key in s l e e p spc 8 ret; do echo "sendkey $$key"; sleep 0.1; done; \
	   for key in n o t i f y spc l a t e spc o n e ret; do echo "sendkey $$key"; sleep 0.1; done; \
	   for key in n o t i f y spc l a t e spc t w o ret; do echo "sendkey $$key"; sleep 0.1; done; sleep 0.3; \
	   echo "mouse_move 280 -248"; sleep 0.2; echo "mouse_move 280 -248"; sleep 0.2; echo "mouse_move 280 -248"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "screendump build/grow-before.ppm"; sleep 6; \
	   echo "screendump build/grow.ppm"; sleep 1; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^control |^notify " build/serial.log || echo "grow-test: no Control Center"
	@grep -q "^control    : 0 recent notifications" build/serial.log \
	    && echo "grow-test: Control Center opened with none             PASS" \
	    || echo "grow-test: Control Center opened with none             FAIL"
	@grep -q "^control    : the card resizes for 2 notification(s)" build/serial.log \
	    && grep -q "^control    : now " build/serial.log \
	    && echo "grow-test: and grew for the two that came after        PASS" \
	    || echo "grow-test: and grew for the two that came after        FAIL"
	@test "$$(grep -c '^control    : told of a new notification' build/serial.log)" = 2 \
	    && echo "grow-test: told of each one as it came, not at 20 s    PASS" \
	    || echo "grow-test: told of each one as it came, not at 20 s    FAIL"
	@test -s build/grow-before.ppm && pnmtopng build/grow-before.ppm > build/grow-before.png 2>/dev/null && rm -f build/grow-before.ppm || true
	@test -s build/grow.ppm && pnmtopng build/grow.ppm > build/grow.png 2>/dev/null && rm -f build/grow.ppm && echo "wrote build/grow.png" || true

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

# Sealing from ring 3: a terminal types `seal` and is refused, holding no
# sealer; then Alt+L, and the lock screen seals and opens a message with
# its own, through the secure core. Return unlocks.
seal-test: iso
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   for key in s e a l ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 1; \
	   echo "sendkey alt-l"; sleep 1.5; \
	   echo "sendkey ret"; sleep 1.2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "seal|^lock " build/serial.log | grep -v "^seal  *:" || echo "seal-test: nothing"
	@grep -q "^terminal   : seal: refused (Empty)" build/serial.log \
	    && echo "seal-test: the terminal holds no sealer and is refused PASS" \
	    || echo "seal-test: the terminal holds no sealer and is refused FAIL"
	@grep -q "^lock       : sealed and opened 23 bytes (51 sealed)" build/serial.log \
	    && ! grep -q "^lock       : sealing:" build/serial.log \
	    && echo "seal-test: the lock screen sealed and opened its own  PASS" \
	    || echo "seal-test: the lock screen sealed and opened its own  FAIL"

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

# More windows at once than the renderer once had surfaces for (16, one
# the wallpaper's): twenty terminals, each opened from the desktop's menu
# (right-click a corner no window reaches, Down to New Terminal, Return),
# build/surfaces.png with all of them up, then Quit from the Terminal
# icon's menu (Up wraps round to Quit) closes every one.
surfaces-test: iso
	@rm -f build/surfaces.ppm build/surfaces.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move 250 -125"; sleep 0.2; echo "mouse_move 250 -125"; sleep 0.5; \
	   for window in $$(seq 20); do \
	       echo "mouse_button 2"; sleep 0.2; echo "mouse_button 0"; sleep 1.2; \
	       echo "sendkey down"; sleep 0.2; echo "sendkey ret"; sleep 1.6; \
	   done; \
	   echo "screendump build/surfaces.ppm"; sleep 2; \
	   echo "mouse_move -250 125"; sleep 0.2; echo "mouse_move -250 125"; sleep 0.2; \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 2"; sleep 0.2; echo "mouse_button 0"; sleep 1.5; \
	   echo "sendkey up"; sleep 0.2; echo "sendkey ret"; sleep 4; echo quit ) \
	  | timeout --foreground 180 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@echo "surfaces-test: at most $$(grep -ao 'opened, [0-9]* open' build/serial.log | awk '{print $$2}' | sort -n | tail -1) windows open; $$(grep -ao 'quit asked [0-9]* window' build/serial.log)"
	@! grep -aq "would not take the surface\|no window" build/serial.log \
	    && grep -aq "quit asked 20 window" build/serial.log \
	    && echo "surfaces-test: twenty terminals open at once             PASS" \
	    || echo "surfaces-test: twenty terminals open at once             FAIL"
	@test "$$(sed -n '/quit asked 20 window/,$$p' build/serial.log | grep -ac 'desktop    : window .* closed')" -ge 20 \
	    && echo "surfaces-test: Quit closed all twenty                    PASS" \
	    || echo "surfaces-test: Quit closed all twenty                    FAIL"
	@test -s build/surfaces.ppm && pnmtopng build/surfaces.ppm > build/surfaces.png 2>/dev/null && rm -f build/surfaces.ppm && echo "wrote build/surfaces.png" || true

# USB: QEMU's xHCI controller with its keyboard and tablet (USB=1's
# devices), driven by QMP rather than the human monitor, since only QMP can
# send a tablet's absolute positions. `qmp` turns lines of the form
# `key a b`, `at X Y` (pixels on the 1280x800 screen, rounded up so the
# kernel's scaling lands on the same pixel), `click`, `shot FILE` and
# `wait S` into commands.
#
# The test types a line, then clicks the panel's terminal button with the
# tablet and types a command into the terminal it opens.
qmp = { echo '{"execute":"qmp_capabilities"}'; \
        while read -r verb a b; do case $$verb in \
          key) for k in $$a $$b; do echo "{\"execute\":\"send-key\",\"arguments\":{\"keys\":[$$(echo $$k | sed 's/-/ /g' | awk '{for(i=1;i<=NF;i++) printf "%s{\"type\":\"qcode\",\"data\":\"%s\"}", (i>1?",":""), $$i}')]}}"; sleep 0.15; done ;; \
          at) echo "{\"execute\":\"input-send-event\",\"arguments\":{\"events\":[{\"type\":\"abs\",\"data\":{\"axis\":\"x\",\"value\":$$(( (a * 32767 + 1278) / 1279 ))}},{\"type\":\"abs\",\"data\":{\"axis\":\"y\",\"value\":$$(( (b * 32767 + 798) / 799 ))}}]}}" ;; \
          click) for down in true false; do echo "{\"execute\":\"input-send-event\",\"arguments\":{\"events\":[{\"type\":\"btn\",\"data\":{\"down\":$$down,\"button\":\"left\"}}]}}"; sleep 0.2; done ;; \
          shot) echo "{\"execute\":\"screendump\",\"arguments\":{\"filename\":\"$$a\"}}" ;; \
          wait) sleep $$a ;; \
        esac; done; echo '{"execute":"quit"}'; }

usb-test: iso
	@rm -f build/usb.ppm build/usb.png
	@( echo "wait $(SHOT_DELAY)"; echo "key $(INPUT_KEYS)"; echo "wait 1"; \
	   echo "at 180 20"; echo "wait 0.5"; echo "click"; echo "wait 2"; \
	   echo "key e c h o spc u s b ret"; echo "wait 1"; \
	   echo "shot build/usb.ppm"; echo "wait 1" ) | $(qmp) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) $(usb_flags) \
	      -serial file:build/serial.log -qmp stdio > /dev/null 2>&1 || true
	@grep -aE "^(usb|xhci) " build/serial.log || echo "usb-test: nothing from USB"
	@grep -aq "^xhci       : controller up: reset, rings set, a no-op command answered" build/serial.log \
	    && echo "usb-test: the controller is up and takes commands     PASS" \
	    || echo "usb-test: the controller is up and takes commands     FAIL"
	@grep -aqE "^xhci       : port [0-9]+: .*, a keyboard: " build/serial.log \
	    && echo "usb-test: the keyboard is addressed and configured    PASS" \
	    || echo "usb-test: the keyboard is addressed and configured    FAIL"
	@grep -aqE "^xhci       : port [0-9]+: .*, a HID device: " build/serial.log \
	    && echo "usb-test: the tablet is addressed and configured      PASS" \
	    || echo "usb-test: the tablet is addressed and configured      FAIL"
	@grep -aqE "^xhci       : slot [0-9]+: the first report, 8 bytes" build/serial.log \
	    && grep -aq '^input      : line "Hi there"' build/serial.log \
	    && echo "usb-test: typing on the USB keyboard reaches the desk PASS" \
	    || echo "usb-test: typing on the USB keyboard reaches the desk FAIL"
	@grep -aqE "^xhci       : slot [0-9]+: reading a tablet's reports" build/serial.log \
	    && grep -aq "^input      : click at (180, 20) on a window" build/serial.log \
	    && echo "usb-test: the tablet clicks where it was put         PASS" \
	    || echo "usb-test: the tablet clicks where it was put         FAIL"
	@grep -aq "^panel      : started a terminal" build/serial.log \
	    && grep -aq '^terminal   : \$$ echo usb' build/serial.log \
	    && echo "usb-test: ...opening a terminal that takes a command PASS" \
	    || echo "usb-test: ...opening a terminal that takes a command FAIL"
	@test -s build/usb.ppm && pnmtopng build/usb.ppm > build/usb.png 2>/dev/null && rm -f build/usb.ppm && echo "wrote build/usb.png" || true

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

# Settings survive a reboot. Boot with a fresh disk, open Settings from the
# dock and pick Light; then boot again with the same disk and touch
# nothing: the desktop comes up light. build/store.png is the second boot.
STORE_TEST_IMG := build/store-test.img
store-test: iso
	@rm -f $(STORE_TEST_IMG) build/store.ppm build/store.png && truncate -s 1M $(STORE_TEST_IMG)
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move 28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   echo "mouse_move -40 -258"; sleep 0.2; echo "mouse_move -41 -258"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) $(call store_flags,$(STORE_TEST_IMG)) \
	      -serial file:build/store-first.log -monitor stdio > /dev/null 2>&1 || true
	@( sleep $(SHOT_DELAY); sleep 1; \
	   echo "screendump build/store.ppm"; sleep 1; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(QEMU_COMMON) $(call store_flags,$(STORE_TEST_IMG)) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -hE "^(store|theme) " build/store-first.log build/serial.log | grep -v PASS || echo "store-test: nothing from the store"
	@grep -q "^store      : the disk keeps no theme yet" build/store-first.log \
	    && echo "store-test: a fresh disk keeps no theme                PASS" \
	    || echo "store-test: a fresh disk keeps no theme                FAIL"
	@grep -q "^store      : kept the theme on the disk" build/store-first.log \
	    && echo "store-test: picking Light keeps it on the disk         PASS" \
	    || echo "store-test: picking Light keeps it on the disk         FAIL"
	@grep -q "^store      : read the theme kept on the disk (light)" build/serial.log \
	    && echo "store-test: and the next boot comes up light           PASS" \
	    || echo "store-test: and the next boot comes up light           FAIL"
	@grep -q "the disk's driver holds its device, its memory, and nothing else[[:space:]]*PASS" build/serial.log \
	    && echo "store-test: the driver holds only its device           PASS" \
	    || echo "store-test: the driver holds only its device           FAIL"
	@test -s build/store.ppm && pnmtopng build/store.ppm > build/store.png 2>/dev/null && rm -f build/store.ppm && echo "wrote build/store.png" || true

# The vault (corrode_vault, kernel/src/vault.rs): the root directory
# sealed on the disk under the passcode, and a boot medium without it.
#
#   ./corrode vault-disk VAULT_PASSCODE=2468   seal root/ into build/vault.img
#   ./corrode run-vault                        boot it: the lock screen first
#
# vault-iso is the ISO's tree less boot/files/root.tar, sealed again by
# scripts/seal-iso.sh, so Limine still checks every file it loads.
VAULT_ISO := build/vault.iso
VAULT_IMG ?= build/vault.img
VAULT_PASSCODE ?=
vault-iso: iso
	@rm -rf build/vault_iso_root $(VAULT_ISO) && cp -r $(ISO_ROOT) build/vault_iso_root
	@rm -f build/vault_iso_root/boot/files/root.tar
	@sed -i '/boot():\/boot\/files\/root.tar/d' build/vault_iso_root/boot/limine/limine.conf
	@LIMINE_DIR=$(LIMINE_DIR) bash scripts/seal-iso.sh build/vault_iso_root $(VAULT_ISO)
	@rm -rf build/vault_iso_root
	@echo "built $(VAULT_ISO): the kernel and programs, no files"

vault-disk: $(ROOT_TAR)
	@test -n "$(VAULT_PASSCODE)" || { echo "vault-disk: give VAULT_PASSCODE=<4 to 32 characters>"; exit 1; }
	@VAULT_TAR=$(CURDIR)/$(ROOT_TAR) VAULT_DISK=$(abspath $(VAULT_IMG)) VAULT_PASSCODE=$(VAULT_PASSCODE) \
	    cargo test -q --release -p corrode-vault --target $(HOST_TARGET) seal_the_root \
	    -- --ignored --nocapture 2>&1 | grep '^vault-disk' \
	    || { echo "vault-disk: could not seal $(ROOT_TAR)"; exit 1; }

run-vault: vault-iso
	@test -f $(VAULT_IMG) || { echo "run-vault: no $(VAULT_IMG); make one with vault-disk"; exit 1; }
	qemu-system-x86_64 $(subst $(ISO),$(VAULT_ISO),$(QEMU_FLAGS)) $(call store_flags,$(VAULT_IMG))

# A fresh vault sealed with Rust4ever -- letters, a capital, a digit -- and
# booted from the vault ISO. The lock screen must be up at boot with no
# files given to anyone; 1111 is refused; Rust4ever, typed with Shift for
# the R, opens it, and a terminal from the dock then has files
# (build/vault-locked.png at boot, build/vault.png after). A second boot of
# a copy with one sealed byte changed: Rust4ever opens nothing.
VAULT_TEST_IMG := build/vault-test.img
VAULT_TEST_KEYS := shift-r u s t 4 e v e r ret
vault-test: vault-iso
	@rm -f $(VAULT_TEST_IMG) build/vault-bad.img build/vault*.ppm build/vault*.png build/vault-*.log
	@$(MAKE) -s vault-disk VAULT_IMG=$(VAULT_TEST_IMG) VAULT_PASSCODE=Rust4ever
	@cp $(VAULT_TEST_IMG) build/vault-bad.img && printf '\x5a' \
	    | dd of=build/vault-bad.img bs=1 seek=$$((4096 + 512 + 700)) conv=notrunc status=none
	@( sleep $(SHOT_DELAY); sleep 1; echo "screendump build/vault-locked.ppm"; sleep 0.5; \
	   for key in 1 1 1 1 ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 2; \
	   for key in $(VAULT_TEST_KEYS); do echo "sendkey $$key"; sleep 0.15; done; sleep 3; \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 3; \
	   echo "screendump build/vault.ppm"; sleep 1; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(subst $(ISO),$(VAULT_ISO),$(QEMU_COMMON)) \
	      $(call store_flags,$(VAULT_TEST_IMG)) -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@( sleep $(SHOT_DELAY); sleep 1; \
	   for key in $(VAULT_TEST_KEYS); do echo "sendkey $$key"; sleep 0.15; done; sleep 3; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(subst $(ISO),$(VAULT_ISO),$(QEMU_COMMON)) \
	      $(call store_flags,build/vault-bad.img) -serial file:build/vault-bad.log -monitor stdio > /dev/null 2>&1 || true
	@grep -ahE "^(vault|files|lock) " build/serial.log | grep -v PASS || echo "vault-test: nothing from the vault"
	@! grep -aq "Loading module .boot():/boot/files/root.tar" build/serial.log \
	    && echo "vault-test: the boot medium carries no files          PASS" \
	    || echo "vault-test: the boot medium carries no files          FAIL"
	@grep -aq "^vault      : the root directory is sealed on the disk" build/serial.log \
	    && grep -aq "^files      : sealed on the disk until the passcode is given" build/serial.log \
	    && grep -aq "^lock " build/serial.log \
	    && echo "vault-test: sealed at boot, lock screen up, no files  PASS" \
	    || echo "vault-test: sealed at boot, lock screen up, no files  FAIL"
	@awk '/^desktop    : a wrong passcode/ {wrong = 1} /^vault      : opened/ {exit !wrong} END {if (!wrong) exit 1}' build/serial.log \
	    && echo "vault-test: a wrong passcode is refused first         PASS" \
	    || echo "vault-test: a wrong passcode is refused first         FAIL"
	@grep -aq "^vault      : opened" build/serial.log \
	    && grep -aq "give the launcher the terminal's files[[:space:]]*PASS" build/serial.log \
	    && grep -aq "^terminal   : [0-9]* files and directories" build/serial.log \
	    && echo "vault-test: the right one opens it; files reach apps  PASS" \
	    || echo "vault-test: the right one opens it; files reach apps  FAIL"
	@grep -aq "^desktop    : a wrong passcode" build/vault-bad.log \
	    && ! grep -aq "^vault      : opened" build/vault-bad.log \
	    && echo "vault-test: one byte changed on disk: nothing opens   PASS" \
	    || echo "vault-test: one byte changed on disk: nothing opens   FAIL"
	@test $$(grep -a "^vault " build/serial.log | grep -ac "PASS") -eq 4 \
	    && ! grep -a "^vault " build/serial.log | grep -aq "FAIL" \
	    && echo "vault-test: the kernel's four vault checks pass       PASS" \
	    || echo "vault-test: the kernel's four vault checks pass       FAIL"
	@for shot in vault-locked vault; do \
	    test -s build/$$shot.ppm && pnmtopng build/$$shot.ppm > build/$$shot.png 2>/dev/null && rm -f build/$$shot.ppm; \
	done; true

# Changing the vault's passcode in Settings. A vault sealed with
# Rust4ever, opened; Settings from the dock; its Passcode row clicked;
# Rust4ever, then Corrode5 twice. build/rekey-settings.png shows the row
# asking, build/rekey.png the answer. The next boot: Rust4ever is refused,
# Corrode5 opens the vault and a terminal has files.
REKEY_IMG := build/rekey-test.img
rekey-test: vault-iso
	@rm -f $(REKEY_IMG) build/rekey*.ppm build/rekey*.png build/rekey-*.log
	@$(MAKE) -s vault-disk VAULT_IMG=$(REKEY_IMG) VAULT_PASSCODE=Rust4ever
	@( sleep $(SHOT_DELAY); sleep 1; \
	   for key in $(VAULT_TEST_KEYS); do echo "sendkey $$key"; sleep 0.15; done; sleep 3; \
	   echo "mouse_move 28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2.5; \
	   echo "mouse_move -184 -105"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 0.5; \
	   for key in $(VAULT_TEST_KEYS); do echo "sendkey $$key"; sleep 0.15; done; sleep 1.5; \
	   echo "screendump build/rekey-settings.ppm"; sleep 0.5; \
	   for key in shift-c o r r o d e 5 ret shift-c o r r o d e 5 ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 4; \
	   echo "screendump build/rekey.ppm"; sleep 0.5; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(subst $(ISO),$(VAULT_ISO),$(QEMU_COMMON)) \
	      $(call store_flags,$(REKEY_IMG)) -serial file:build/rekey-first.log -monitor stdio > /dev/null 2>&1 || true
	@( sleep $(SHOT_DELAY); sleep 1; \
	   for key in $(VAULT_TEST_KEYS); do echo "sendkey $$key"; sleep 0.15; done; sleep 2; \
	   for key in shift-c o r r o d e 5 ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 3; \
	   echo "mouse_move -28 179"; sleep 0.2; echo "mouse_move 0 179"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.2; echo "mouse_button 0"; sleep 3; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(subst $(ISO),$(VAULT_ISO),$(QEMU_COMMON)) \
	      $(call store_flags,$(REKEY_IMG)) -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -ahE "^(vault|settings|desktop) " build/rekey-first.log build/serial.log | grep -v PASS || echo "rekey-test: nothing"
	@grep -aq "^vault      : sealed again under the new passcode" build/rekey-first.log \
	    && grep -aq "^settings   : the passcode changed" build/rekey-first.log \
	    && echo "rekey-test: Settings changed it, the old one first    PASS" \
	    || echo "rekey-test: Settings changed it, the old one first    FAIL"
	@awk '/^desktop    : a wrong passcode/ {wrong = 1} /^vault      : opened/ {exit !wrong} END {if (!wrong) exit 1}' build/serial.log \
	    && echo "rekey-test: next boot, the old passcode is refused    PASS" \
	    || echo "rekey-test: next boot, the old passcode is refused    FAIL"
	@grep -aq "^vault      : opened" build/serial.log \
	    && grep -aq "^terminal   : [0-9]* files and directories" build/serial.log \
	    && echo "rekey-test: and the new one opens the files           PASS" \
	    || echo "rekey-test: and the new one opens the files           FAIL"
	@for shot in rekey-settings rekey; do \
	    test -s build/$$shot.ppm && pnmtopng build/$$shot.ppm > build/$$shot.png 2>/dev/null && rm -f build/$$shot.ppm; \
	done; true

# A passcode in the clear in desktop.conf, rewritten as a salted hash, in
# place; the rest of the file is kept as it was. PASSCODE_CONF names
# another file.
PASSCODE_CONF ?= $(CURDIR)/root/etc/desktop.conf
passcode-hash:
	@PASSCODE_CONF=$(PASSCODE_CONF) cargo test -q --release -p corrode-theme \
	    --target $(HOST_TARGET) hash_the_passcode -- --ignored --nocapture 2>&1 | grep '^passcode-hash'

# The lock screen with a passcode. A copy of the ISO whose desktop.conf
# has `passcode = 2468` (build/passcode.iso; the real one is untouched),
# hashed by passcode-hash first unless PASSCODE_FORM=plain.
# Lock with Alt+L; a wrong code is refused, the right one unlocks. Lock
# again: three wrong in a row and even the right one is refused for a
# while. build/passcode-dots.png shows two digits typed, build/passcode.png
# the wait.
PASS_ISO := build/passcode.iso
passcode-test: iso
	@rm -rf build/pass_root build/pass_iso_root $(PASS_ISO) build/passcode.ppm build/passcode.png build/passcode-dots.ppm build/passcode-dots.png
	@cp -r root build/pass_root && printf '\npasscode = 2468\n' >> build/pass_root/etc/desktop.conf
	@if [ "$(PASSCODE_FORM)" != plain ]; then $(MAKE) -s passcode-hash PASSCODE_CONF=$(CURDIR)/build/pass_root/etc/desktop.conf; fi
	@cp -r $(ISO_ROOT) build/pass_iso_root
	@tar --format=ustar --sort=name --owner=0 --group=0 --numeric-owner \
	    --mtime=@0 --mode=u+rw,go+r,go-w -C build/pass_root -cf build/pass_iso_root/boot/files/root.tar .
	@LIMINE_DIR=$(LIMINE_DIR) bash scripts/seal-iso.sh build/pass_iso_root $(PASS_ISO)
	@( sleep $(SHOT_DELAY); \
	   echo "sendkey alt-l"; sleep 1.5; \
	   echo "sendkey ret"; sleep 0.3; \
	   for key in 1 1 1 1 ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 0.5; \
	   echo "sendkey 2"; sleep 0.15; echo "sendkey 4"; sleep 0.5; \
	   echo "screendump build/passcode-dots.ppm"; sleep 1; \
	   echo "sendkey backspace"; sleep 0.15; echo "sendkey backspace"; sleep 0.15; \
	   for key in 2 4 6 8 ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 1.5; \
	   echo "sendkey alt-l"; sleep 1.5; \
	   for round in 1 2 3; do for key in 1 1 1 1 ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 0.3; done; \
	   for key in 2 4 6 8 ret; do echo "sendkey $$key"; sleep 0.15; done; sleep 0.8; \
	   echo "screendump build/passcode.ppm"; sleep 1; echo quit ) \
	  | timeout --foreground 90 qemu-system-x86_64 $(subst $(ISO),$(PASS_ISO),$(QEMU_COMMON)) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@grep -E "^lock |passcode" build/serial.log || echo "passcode-test: no lock screen"
	@grep -q "^theme      : read /etc/desktop.conf, with a lock screen passcode" build/serial.log \
	    && echo "passcode-test: the passcode is read                     PASS" \
	    || echo "passcode-test: the passcode is read                     FAIL"
	@if [ "$(PASSCODE_FORM)" = plain ]; then want='in the clear'; else want=hashed; fi; \
	    ! grep -q '^passcode = 2468' build/pass_root/etc/desktop.conf || [ "$(PASSCODE_FORM)" = plain ] \
	    && grep -q "with a lock screen passcode ($$want" build/serial.log \
	    && printf 'passcode-test: %-41sPASS\n' "read $$want, kept as a hash" \
	    || printf 'passcode-test: %-41sFAIL\n' "read $$want, kept as a hash"
	@test "$$(grep -c '^lock       : wrong passcode' build/serial.log)" = 4 \
	    && echo "passcode-test: four wrong codes are refused             PASS" \
	    || echo "passcode-test: four wrong codes are refused             FAIL"
	@test "$$(grep -c '^lock       : unlocked' build/serial.log)" = 1 \
	    && echo "passcode-test: the right one unlocks, once              PASS" \
	    || echo "passcode-test: the right one unlocks, once              FAIL"
	@grep -q "^lock       : too many tries" build/serial.log \
	    && echo "passcode-test: after three wrong, even it must wait     PASS" \
	    || echo "passcode-test: after three wrong, even it must wait     FAIL"
	@test -s build/passcode-dots.ppm && pnmtopng build/passcode-dots.ppm > build/passcode-dots.png 2>/dev/null && rm -f build/passcode-dots.ppm || true
	@test -s build/passcode.ppm && pnmtopng build/passcode.ppm > build/passcode.png 2>/dev/null && rm -f build/passcode.ppm && echo "wrote build/passcode.png" || true

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
# Windows programs from the terminal (`win`): reader.exe by its full path;
# again from its folder, by a relative one; and a file that is not a
# program. Run from its folder it sees that folder alone, so of all it
# looks for, only itself is there. build/win.png is the terminal after.
WIN_KEYS := w i n spc slash p r o g r a m s slash r e a d e r slash r e a d e r dot e x e \
            spc o n e spc shift-apostrophe t w o spc t h r e e shift-apostrophe ret
WIN_AGAIN_KEYS := c d spc slash p r o g r a m s slash r e a d e r ret \
                  w i n spc r e a d e r dot e x e ret
WIN_BAD_KEYS := w i n spc slash e t c slash m o t d ret
WIN_SPIN_KEYS := w i n spc slash p r o g r a m s slash s p i n slash s p i n dot e x e ret
# dir.exe, compiled: its whole folder (C:\, a root, so no . or ..), then a
# pattern nothing matches.
WIN_DIR_KEYS := w i n spc slash p r o g r a m s slash d i r slash d i r dot e x e ret
WIN_DIR_NONE_KEYS := up spc x shift-8 dot t x t ret
# fault.exe, which reads address 0.
WIN_FAULT_KEYS := w i n spc slash p r o g r a m s slash f a u l t slash f a u l t dot e x e ret
# hello.exe, a C program compiled by mingw-w64's gcc (verdigris/c/hello.c):
# through msvcrt.dll's start-up, with arguments, then without.
WIN_C_KEYS := w i n spc slash p r o g r a m s slash c slash h e l l o dot e x e \
              spc o n e spc shift-apostrophe t w o spc t h r e e shift-apostrophe ret
WIN_C_BARE_KEYS := w i n spc slash p r o g r a m s slash c slash h e l l o dot e x e ret
# Frames a `win` run may keep once the next one has started: none. Its
# tasks are given back, and so is the memory it and its view shared.
WIN_RUN_FRAMES ?= 0
win-test: iso
	@rm -f build/win.ppm build/win.png
	@( sleep $(SHOT_DELAY); \
	   echo "mouse_move 120 -80"; sleep 0.2; \
	   echo "mouse_move -250 -150"; sleep 0.2; echo "mouse_move -250 -150"; sleep 0.2; \
	   echo "mouse_move -80 0"; sleep 0.5; \
	   echo "mouse_button 1"; sleep 0.3; echo "mouse_button 0"; sleep 2; \
	   for key in $(WIN_KEYS); do echo "sendkey $$key"; sleep 0.12; done; sleep 3; \
	   for key in $(WIN_AGAIN_KEYS); do echo "sendkey $$key"; sleep 0.12; done; sleep 3; \
	   for key in $(WIN_BAD_KEYS); do echo "sendkey $$key"; sleep 0.12; done; sleep 1.5; \
	   for key in up up ret; do echo "sendkey $$key"; sleep 0.12; done; sleep 2.5; \
	   for run in 1 2 3 4 5; do for key in up ret; do echo "sendkey $$key"; sleep 0.12; done; sleep 2.5; done; \
	   for key in $(WIN_SPIN_KEYS); do echo "sendkey $$key"; sleep 0.12; done; sleep 13; \
	   for key in up up ret; do echo "sendkey $$key"; sleep 0.12; done; sleep 2.5; \
	   for key in $(WIN_DIR_KEYS); do echo "sendkey $$key"; sleep 0.12; done; sleep 2.5; \
	   for key in $(WIN_DIR_NONE_KEYS); do echo "sendkey $$key"; sleep 0.12; done; sleep 2.5; \
	   for key in $(WIN_FAULT_KEYS); do echo "sendkey $$key"; sleep 0.12; done; sleep 2.5; \
	   for key in $(WIN_C_KEYS); do echo "sendkey $$key"; sleep 0.12; done; sleep 3; \
	   for key in $(WIN_C_BARE_KEYS); do echo "sendkey $$key"; sleep 0.12; done; sleep 3; \
	   echo "screendump build/win.ppm"; sleep 1; echo quit ) \
	  | timeout --foreground 240 qemu-system-x86_64 $(QEMU_COMMON) \
	      -serial file:build/serial.log -monitor stdio > /dev/null 2>&1 || true
	@tr -d '\r' < build/serial.log > build/win-lines.log
	@grep -a "^terminal   : win\|^desktop    : /programs\|^desktop    : task" build/win-lines.log || echo "win-test: nothing from win"
	@test $$(grep -acxF 'desktop    : /programs/reader/reader.exe started for the terminal, seeing /programs/reader alone' build/win-lines.log) -eq 9 \
	    && test $$(grep -acE '^terminal   : win: reader.exe ended after [0-9]+ lines$$' build/win-lines.log) -eq 9 \
	    && echo "win-test: the terminal ran it 9 times, to its end    PASS" \
	    || echo "win-test: the terminal ran it 9 times, to its end    FAIL"
	@grep -aqxF 'terminal   : win: spin.exe: spinning, and never ending' build/win-lines.log \
	    && grep -aqxF 'terminal   : win: spin.exe ran 10 seconds and was stopped' build/win-lines.log \
	    && echo "win-test: a program that never ends is stopped      PASS" \
	    || echo "win-test: a program that never ends is stopped      FAIL"
	@grep -aqxF 'terminal   : win: reader.exe: C:\*: reader.exe' build/win-lines.log \
	    && grep -aqxF 'terminal   : win: reader.exe: current directory: C:\' build/win-lines.log \
	    && grep -aqxF 'terminal   : win: reader.exe: keys.txt: not there (2, ERROR_FILE_NOT_FOUND)' build/win-lines.log \
	    && grep -aqxF 'terminal   : win: reader.exe: c:/Etc/MOTD: not there (3, ERROR_PATH_NOT_FOUND)' build/win-lines.log \
	    && echo "win-test: its output came back; it saw its folder alone PASS" \
	    || echo "win-test: its output came back; it saw its folder alone FAIL"
	@grep -aqxF 'terminal   : win: reader.exe: command line: C:\reader.exe one "two three"' build/win-lines.log \
	    && test $$(grep -acxF 'terminal   : win: reader.exe: command line: C:\reader.exe' build/win-lines.log) -eq 8 \
	    && echo "win-test: it gets its command line, quoted as typed PASS" \
	    || echo "win-test: it gets its command line, quoted as typed FAIL"
	@size=$$(stat -c %s $(DIR_EXE)) \
	    && grep -aqxF "terminal   : win: dir.exe: dir.exe  $$size" build/win-lines.log \
	    && grep -aqxF "terminal   : win: dir.exe: 1 files, $$size bytes" build/win-lines.log \
	    && grep -aqxF 'terminal   : win: dir.exe: dir: x*.txt: error 2' build/win-lines.log \
	    && grep -aqxF 'terminal   : win: dir.exe ended after 1 lines, exit code 1' build/win-lines.log \
	    && echo "win-test: a compiled program lists its folder      PASS" \
	    || echo "win-test: a compiled program lists its folder      FAIL"
	@grep -aqxF 'terminal   : win: fault.exe: reading address 0' build/win-lines.log \
	    && grep -aqxF 'terminal   : win: fault.exe faulted: access violation (0xc0000005), after 1 lines' build/win-lines.log \
	    && echo "win-test: a fault is told as Windows tells it      PASS" \
	    || echo "win-test: a fault is told as Windows tells it      FAIL"
	@c='terminal   : win: hello.exe: '; \
	grep -aqxF "$${c}hello from C: 3 arguments" build/win-lines.log \
	    && grep -aqxF "$${c}argv[0] = [C:\hello.exe]" build/win-lines.log \
	    && grep -aqxF "$${c}argv[1] = [one]" build/win-lines.log \
	    && grep -aqxF "$${c}argv[2] = [two three]" build/win-lines.log \
	    && grep -aqxF 'terminal   : win: hello.exe ended after 9 lines, exit code 2' build/win-lines.log \
	    && grep -aqxF "$${c}hello from C: 1 arguments" build/win-lines.log \
	    && grep -aqxF 'terminal   : win: hello.exe ended after 7 lines' build/win-lines.log \
	    && echo "win-test: a C program gets argc, argv and its exit code PASS" \
	    || echo "win-test: a C program gets argc, argv and its exit code FAIL"
	@c='terminal   : win: hello.exe: '; \
	test $$(grep -acxF "$${c}printf:    42|-7   |003.1|beef|ok" build/win-lines.log) -eq 2 \
	    && test $$(grep -acxF "$${c}fprintf: +7 4000000000 0002.500 1.234568e+04 0.0001 Z    ab xy %" build/win-lines.log) -eq 2 \
	    && test $$(grep -acxF "$${c}heap: malloc and free, calloc zeroed yes, last byte 64" build/win-lines.log) -eq 2 \
	    && test $$(grep -acxF "$${c}thread-local: 42" build/win-lines.log) -eq 2 \
	    && test $$(grep -acxF "$${c}atexit ran" build/win-lines.log) -eq 2 \
	    && echo "win-test: ...and its C runtime prints, allocates, exits PASS" \
	    || echo "win-test: ...and its C runtime prints, allocates, exits FAIL"
	@test $$(grep -acE '^verdigris  : moved from 0x140000000 to 0x400000, [1-9][0-9]* relocations$$' build/win-lines.log) -eq 4 \
	    && echo "win-test: it was moved from where its linker put it PASS" \
	    || echo "win-test: it was moved from where its linker put it FAIL"
	@grep -aqxF 'terminal   : win: /etc/motd: not a program Verdigris runs' build/win-lines.log \
	    && echo "win-test: a file that is not a program is refused   PASS" \
	    || echo "win-test: a file that is not a program is refused   FAIL"
	@grep -aE "^desktop    : [0-9]+ frames (free|of)" build/win-lines.log | grep -oE '[0-9]+ frames( free)?' | tr '\n' ' '; echo
	@grep -aoE '^desktop    : [0-9]+ frames of finished' build/win-lines.log | grep -oE '[0-9]+' \
	    | awk '{ back += $$1 } END { exit back < 7 * 100 }' \
	    && echo "win-test: each run gets back the last one's tasks   PASS" \
	    || echo "win-test: each run gets back the last one's tasks   FAIL"
	@# reader.exe's runs, the last after spin.exe was stopped and reaped.
	@grep -aA1 -xF 'desktop    : /programs/reader/reader.exe started for the terminal, seeing /programs/reader alone' build/win-lines.log \
	    | grep -aoE '^desktop    : [0-9]+ frames free' | grep -oE '[0-9]+' | tail -n +2 \
	    | awk 'NR > 1 && last - $$1 > $(WIN_RUN_FRAMES) { bad = 1 } { last = $$1 } END { exit bad || NR != 8 }' \
	    && echo "win-test: a run keeps no memory once it is over     PASS" \
	    || echo "win-test: a run keeps no memory once it is over     FAIL"
	@test $$(grep -aoE ', [0-9]+ memory objects$$' build/win-lines.log | sort -u | wc -l) -eq 1 \
	    && echo "win-test: the memory objects in use stay as many    PASS" \
	    || echo "win-test: the memory objects in use stay as many    FAIL"
	@test -s build/win.ppm && pnmtopng build/win.ppm > build/win.png 2>/dev/null && rm -f build/win.ppm; true

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
