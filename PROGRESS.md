# Desktop progress log

Work on the `desktop-glass` branch while you were away. Newest entries at
the bottom. Every entry was built (`./corrode iso`), unit-tested
(`./corrode test`) and booted in QEMU (`./corrode input-test` and friends)
before it was committed.

Choices made without asking are marked **Choice:**.

## Setup

- Arch had no podman (the Nobara box did). Installed `podman` with pacman
  and rebuilt the `corrode-dev` image. Nothing else on the host changed.
- Old host-target test binaries in `target/x86_64-unknown-linux-gnu` were
  linked against the old image's glibc and failed to start; deleted that
  cache directory (build output only).
- **Choice:** the repo was on `secure-core/reserve`, not `main`, so
  `desktop-glass` branches from there.
- **Choice:** the liquid-glass site is React + TypeScript over SVG filters.
  CorrodeOS has no JavaScript engine and no browser, so the TypeScript
  cannot run there. The optics were ported to Rust instead (MIT,
  attributed in `render/src/glass.rs`).

## 1. Close buttons, and glass in the renderer

- Every title bar has a close button. It sends `window::CLOSE`; the
  terminal exits on it (and on a new `exit` command). The window manager
  drops any window whose program has finished, so a crashed program's
  window goes too. `./corrode close-test` checks both paths. PASS.
- `render/src/glass.rs`: integer port of liquid-glass's edge refraction,
  Regular/Clear tone tables, frost (three-pass box blur standing in for
  each Gaussian), tint and rim light. Nine host tests check it against
  the library's float formulas.
- `Command::Glass` and `Content::Glass`. A pane reads around itself, so
  the scene repaints a pane whole, with its reach, whenever damage touches
  it, which means it never reads its own last frame.
- `Command::Image` gains `translucent`: the top byte of a pixel is
  transparency (0 = opaque, so every existing program is unchanged).
- Known gap: the virtio-gpu ring-3 driver gets no scratch memory, so with
  `GPU=virtio` panes fall back to a plain veil. The default VGA path has
  full glass.

## 2. Glass on screen

- The kernel maps 3 screens of pixels for glass scratch, beside the back
  buffer.
- `window::TRANSLUCENT` flag; the panel now opens translucent and draws
  on clear pixels over a pane of Regular dark glass.
- A dock pane floats above the bottom edge (empty for now).
- `ui` text blending is straight-alpha aware, so text on clear pixels has
  no dark fringe.
- Screenshot: `build/progress-1-glass.png`. Glass renders with its rim; the
  boot screen behind is flat dark, so refraction is hard to see until there
  is a wallpaper.

## 3. desktop.conf and presets

- New crate `theme/` (host-tested, 7 tests): parses `key = value` lines,
  presets first, unknown keys and bad values skipped line by line. Packs
  a whole theme into four IPC words and back losslessly.
- `root/etc/desktop.conf` is the one file to edit, commented throughout.
  Presets: `corrode` (default: dark glass, rust accent, macOS traffic
  lights, Plasma-ish panel), `tahoe`, `tahoe-light`, `breeze` (buttons on
  the right, ribbon wallpaper), `crystal` (Clear glass, strong refraction),
  `plain` (no glass, shadows or animation).
- The kernel reads it at boot from root.tar (`kernel/src/look.rs`) and
  keeps the current theme. Every window is sent a `window::THEME` event
  when it opens; the panel recolours itself from it.
- **Choice:** a `#` only starts a comment at the start of a line or after
  whitespace when followed by whitespace, so `accent = #e2643a` works.
- Boot: `theme : read /etc/desktop.conf`, 170 checks PASS, 0 FAIL.

## 4. Wallpaper, glass menu bar, translucent panel

- `theme/src/wallpaper.rs`: four wallpapers drawn from the theme's three
  colours in integer arithmetic (aurora glows, Breeze-style ribbons,
  layered dunes, plain gradient), light and dark, with a Bayer dither
  against banding. Host-tested.
- The kernel paints the wallpaper into its own memory and shows it as the
  renderer's last surface (`surface::WALLPAPER`; 16 surfaces now, 15 for
  windows). It fades in 1.2 s after boot; once it covers the boot screen,
  the boot screen's layers, its script and its typed line go, so nothing
  moves behind the desktop.
- Boot checks are unchanged: they all run before the desktop takes over.
  170 PASS, 0 FAIL.
- Scenes now reuse a removed layer's slot once they are full and the
  layer has been painted away. Without that, opening and closing windows
  would fill the scene for good. New test for it.
- The painter's scene grew to 160 layers. That is 84 KiB, more than
  Limine's 64 KiB boot stack, so `Scene::boxed` (behind an `alloc`
  feature) builds it straight onto the kernel heap.
- Known gap: under `GPU=virtio` there is no wallpaper (the remote renderer
  only has programs' surfaces); the plain background shows instead.
- Screenshot: `build/progress-3-wallpaper.png`.

## 5. Window chrome

- Every window is now: a soft shadow, a glass frame behind the title bar
  and body (rounded to `corner_radius`, tinted with the accent while it has
  the keyboard), macOS's three traffic lights, a centred bold title, and
  the program's pixels with their lower corners rounded to match.
- Unfocused windows get grey lights, a dimmer title and a lighter shadow.
- Hovering the lights shows x / - / + glyphs on them, macOS-style; one
  shared set of glyph layers moves to whichever window is hovered.
- `buttons = right` (the `breeze` preset) puts them on the right, Plasma's
  order: minimize, zoom, close.
- A program names its window in `OPEN`'s fourth word (8 ASCII bytes);
  `Window::open_titled` in the ui crate. The terminal is "Terminal".
- Renderer: `Command::Shadow` (rounded-rect falloff, cut out where the
  window is, so glass is never darkened) and a lower-corner radius on
  `Command::Image`.
- The red light is the close button now; `close-test` aims at it. PASS.
- **Choice:** the title bar is 28 px, lights 12 px, 20 px apart, per the
  macOS metrics in the apple-design reference.
- Known gap: minimize and zoom do nothing yet (next item).
- Known gap: the shadow has no drop. It is cut out by its own shape, so
  dropping it would leave an unshaded strip under the window.
- Screenshot: `build/progress-4-chrome.png`.

## 6. The dock

- `kernel/src/dock.rs`: a glass pane centred above the bottom edge with
  an icon per program the launcher knows: Terminal (`>_`), Settings (a
  cog), About (`i`). Icons are superellipse tiles (Apple's icon shape)
  drawn as atlas stencils with a white glyph over them; size from
  `dock_size`.
- Clicking an icon asks the launcher through the same endpoint the panel
  holds, so the dock can start nothing the panel could not. The icon
  jumps on a bouncy spring while it starts (off with `bounce = off`,
  `animation_speed = 0`, or the `breeze` and `plain` presets).
- A dot under an icon while that program has a window open (matched by
  window title).
- The launcher now knows program numbers 1 terminal, 2 settings,
  3 about. Settings and About do not exist yet, so the launcher refuses
  them cleanly ("would not start Settings (9)"). They are the next items.
- `./corrode dock-test` starts a terminal from the dock. PASS.
  `close-test` PASS, boot 170 PASS / 0 FAIL.
- **Choice:** the painter calls the launcher synchronously, so the desktop
  pauses for the few milliseconds a program takes to load. Fine in QEMU;
  worth a queue if it ever shows.
- Screenshot: `build/progress-5-dock.png`.

## 7. Minimize and zoom

- **Yellow** minimizes: the window sinks towards the dock and fades
  (280 ms ease-in, scaled by `animation_speed`), and gives up the
  keyboard. Clicking its dock icon brings it back on a spring and focuses
  it. Clicking the icon of a program whose window is already open now
  focuses that window instead of starting a second copy, as macOS does.
- **Green** zooms: windows can't be resized yet (a program's pixels are a
  fixed-size surface), so for now zoom centres the window between the
  panel and the dock, and a second click sends it back where it was. Moves
  use a snappy spring.
- Fixed: the hover glyphs (x - +) stayed behind when a window moved,
  minimized or closed. They are now hidden whenever their buttons move.
- `./corrode minimize-test` minimizes, restores from the dock and zooms.
  PASS. `close-test` and `dock-test` PASS; boot 170 PASS / 0 FAIL.
- **Choice:** there is no genie effect. The scene can move and fade layers
  but not warp them, so minimize is a sink and fade.
- Known gap: real zoom-to-fill needs surfaces that programs can resize.
  That needs a RESIZE event plus surface reallocation; noted for later.
- Screenshot: `build/progress-6-zoom.png`.

## 8. Settings (live) and About

- **Settings** (`user/settings`, the cog in the dock) is a System
  Settings-style inset card with one row per `desktop.conf` setting:
  preset, appearance, accent (eight macOS swatches), glass, wallpaper,
  buttons (macOS left / Plasma right), corners, dock, shadows and motion.
- A click sends the whole theme to the window server as a new
  `window::SET_THEME` message. The painter notices the change on its next
  frame and:
  - repaints the wallpaper;
  - re-makes every window's glass frame, corner radius, button order,
    title colour and shadow;
  - rebuilds the dock (size, on/off);
  - sends `THEME` to every program, so the panel, the terminal and
    Settings itself redraw in the new colours.
- Settings redraws only when that echo arrives, so it always shows what
  the desktop actually has.
- **About** (`user/about`, the `i` in the dock): an accent-coloured logo
  tile, the name and version, a few facts, and a live uptime line.
- `ui` crate: `Window::rounded()` (antialiased rounded rect, blended),
  `corrode_ui::set_theme()`, and a re-export of the theme crate.
- Shadows always exist now, at opacity 0 when `shadows = off`, so toggling
  them live puts them back under the window. They are lighter on a light
  desktop.
- Fixed: a minimized window's title and shadow came back when focus moved
  (`paint_focus` now leaves minimized windows alone).
- `./corrode settings-test` opens Settings from the dock, switches to
  Light, Dunes and a blue accent, then opens About. PASS. All other boot
  tests PASS; 170 PASS / 0 FAIL.
- **Choice:** any program with a window may change the look. It is
  cosmetic, and a separate capability for it felt like overkill for now.
- Known gap: changes are not saved. The root filesystem is a read-only
  tar on the ISO, so a reboot goes back to `/etc/desktop.conf`.
- Known gap: repainting the wallpaper takes a noticeable moment (it is
  computed per pixel on the painter). Fine for a settings click.
- Screenshot: `build/progress-7-settings.png`.

## 9. README: customizing

- New README section "Customizing the desktop":
  - the three ways to change the look (Settings live, `desktop.conf` at
    boot, presets and wallpapers in code);
  - a table of every `desktop.conf` key;
  - a three-line example;
  - how programs follow the look (`Event::Theme`) and change it
    (`corrode_ui::set_theme`).
- Verified the example: booted with just `preset = breeze`,
  `accent = #3daee9` and `corner_radius = 6`. It came up with Plasma-order
  buttons on the right, the ribbons wallpaper, a blue accent and 6 px
  corners. Screenshot: `build/progress-8-breeze.png`. Then restored the
  default `desktop.conf`.
- **The original backlog is done.** Next steps, chosen by me, are below.
