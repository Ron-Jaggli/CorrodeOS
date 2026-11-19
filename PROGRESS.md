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

## 10. Closed windows give their surface back

- Problem: there are 15 window surfaces and none was ever freed, so about
  the 15th window of a session could not open.
- The frame allocator has no `free`, so a closed window's surface is
  *retired* rather than freed, and the next window that fits takes it
  over. Its frames are wiped first, so nothing of the last program shows.
- The painter retires a surface only after:
  - its program has finished (the dead task never runs again, so its old
    mapping of those frames is harmless);
  - the window's layers have left the scene;
  - the software renderer has detached it;
  - bootstrap's capability to it has been deleted, so the number can be
    issued again.
- `./corrode churn-test` opens a terminal from the dock and types `exit`,
  20 times. 20 started, 20 closed, PASS. Every other boot test PASS;
  170 PASS / 0 FAIL.
- Known gap: under `GPU=virtio` the driver keeps every surface it was
  shown mapped, so surfaces are not retired there. Windows still run out
  after about 14.
- Known gap: each program start still uses up frames for its address
  space and stacks, which are never freed (no frame free in the kernel).
  Fine for hundreds of launches, not for ever.

## 11. The menu bar names the app in front

- New `window::ACTIVE` event for docked windows: the title of the window
  with the keyboard, or none. The desk checks once per poll whether it
  changed, and re-sends when a new docked window turns up.
- The panel shows the name in bold after its Terminal button, as macOS's
  menu bar shows the frontmost app. It clears when nothing has focus (a
  click on the desktop, or the window minimized or closed).
- **Choice:** the Terminal button stays where it was, so existing click
  tests keep their coordinates. The name goes after it, not before as on
  macOS.
- Boot tests all PASS; 170 PASS / 0 FAIL.
- Screenshot: `build/progress-9-menubar.png` ("About" in the bar).

## 12. Keyboard shortcuts

- Alt stands in for Cmd. The desktop takes these presses before the
  window in front sees them:
  - **Alt+Tab:** next window, restored if minimized;
  - **Alt+Q / Alt+W / Alt+F4:** close the window in front (sends `CLOSE`,
    as the red light does);
  - **Alt+M:** minimize;
  - **Alt+Z:** zoom;
  - **Alt+,** : Settings (focus it if open, start it from the dock if not).
- Documented in the README's customizing section.
- `./corrode shortcut-test` drives all five with QEMU `sendkey`. PASS.
  Every other boot test PASS; 170 PASS / 0 FAIL.
- **Choice:** Alt, not a Super/Windows key. The PS/2 decoder has no
  Super modifier yet, and Alt is what Plasma uses.
- Known gap: Alt+Tab switches at once. There is no macOS-style switcher
  overlay while Alt is held.
- Known gap: Alt+, needs the dock (`dock = off` means it can only focus a
  Settings window that is already open).
- Screenshot: `build/progress-10-shortcuts.png`.

## 13. Dock hover

- The icon under the pointer lifts 6 px on a snappy spring, and its name
  appears above it on a small glass pill (fades in over 120 ms), as
  macOS's dock labels icons. A launch bounce lands at the lifted height
  while the pointer is still on the icon.
- Hidden tooltips are skipped by hit-testing (the scene ignores layers at
  alpha 0), so they never swallow clicks on windows behind them.
- **Choice:** no magnification. Mask stencils are drawn at one size and
  the renderer has no scaling, so lift plus label gives the feedback
  without it.
- All boot tests PASS; 170 PASS / 0 FAIL.
- Screenshot: `build/progress-11-dock-hover.png` (Settings hovered, and
  dark-mode Settings open).

## 14. Glass and wallpaper on the virtio GPU

- With `GPU=virtio` the ring-3 driver does the drawing. It had no memory
  for glass (so panes fell back to a veil) and could not show a wallpaper.
- New optional capability `gpu::SCRATCH`, 32 MiB of memory (enough for
  1920x1080). The driver splits it into the glass's scratch and the
  wallpaper's pixels.
- New message `gpu::WALLPAPER` carrying the theme's four words. The
  driver paints the wallpaper itself with `corrode-theme`, in its own
  memory, and attaches it as the last surface. Live theme changes work the
  same way.
- The driver logs what it got ("glass and a wallpaper, in 32784 KiB of
  scratch"). Without the memory it carries on as before.
- Bug found on the way: slot 46 collided with the NVIDIA firmware blobs,
  which fill slots upward from 45. Moved to 62.
- `GPU=virtio ./corrode settings-test` PASS, and its screenshot matches
  the software path. Boot under virtio 177 PASS / 0 FAIL; default path
  170 / 0; every other boot test PASS.
- Still open under virtio: closed windows' surfaces are not reused (the
  driver keeps them mapped).
- Screenshot: `build/progress-12-virtio.png`.

## 15. Zoom fills the screen (resizable windows)

- New `OPEN` flag `window::RESIZABLE` and event `window::RESIZE`
  (`width`, `height`).
  - A resizable window's surface is allocated with room for the whole
    screen up front.
  - A resize only changes its logical size (rows are `width` pixels
    apart), within frames that are already mapped. Nothing is remapped or
    reallocated.
- `corrode_ui`: `Window::wait` now takes `&mut self` and applies a resize
  before returning `Event::Resize`. The terminal opts in and simply
  redraws, since it already lays itself out from the window's size.
- Green (and Alt+Z) on a resizable window fills the room between the
  panel and the dock (1264x640 on a 1280x800 screen); pressing it again
  puts back the old size and place. Every part of the window follows: the
  frame, shadow and body sizes, the title re-centred, Plasma-side buttons
  re-placed. Non-resizable windows (Settings, About) still just centre.
- Window size limit raised from 1024x768 to 1920x1200.
- `minimize-test` now checks 1264x640 on zoom and 600x340 on unzoom.
  PASS. Every other boot test PASS; 170 PASS / 0 FAIL.
- Known gap: under `GPU=virtio` zoom only centres. The driver would need
  telling the new size of a surface it already maps. Its boot self-checks
  are still 0 FAIL, but `minimize-test`'s size checks fail there by design.
- Known gap: no dragging window edges to resize. Only zoom resizes.
- Screenshot: `build/progress-13-zoom-fill.png`.

## 16. Resize by dragging an edge

- Press within 8 px of a resizable window's right edge, bottom edge or
  bottom-right corner and drag: the window follows.
  - The program hears nothing of that press.
  - The new size is applied at most once per frame, however many pointer
    moves arrived, so the program redraws per frame, not per move.
  - Limits: no smaller than 240x120, and no further than the screen's
    edge.
- A hand resize forgets the zoom's saved size, so green zooms from the
  new size.
- `./corrode resize-test` drags a terminal's corner from 600x340 to
  800x440. PASS. All other boot tests PASS; 170 PASS / 0 FAIL.
- Known gaps:
  - no resize cursor (the pointer has one shape);
  - no left or top edges;
  - like zoom, only on the software renderer.
- Screenshot: `build/progress-14-resize.png`.

## 17. A real clock in the menu bar

- `kernel/src/rtc.rs` reads the CMOS real-time clock once at boot. It
  waits out an update in progress, re-reads until two readings agree, and
  handles BCD and 12-hour modes. It pairs the result with the monotonic
  clock ("rtc : 2026-11-19 07:13:01 UTC" in the log).
- New event `window::CLOCK` (wall seconds plus the monotonic nanosecond)
  goes to docked windows. The panel keeps time from that pair and redraws
  at each minute boundary.
- The menu bar shows "Thu 19 Nov  07:13", right-aligned, as macOS does.
  Uptime is shown only before the clock arrives (it is still in About).
- `theme/src/clock.rs`: date formatting with no allocation, host-tested
  (leap day, a timezone that crosses midnight, 12-hour AM/PM).
- New `desktop.conf` keys `timezone` (`UTC`, `+5`, `-8`, `+05:30`) and
  `clock` (`24h`/`12h`), packed into the theme word's spare bits. Settings
  gained a Clock row. Picking a preset in Settings keeps your timezone and
  clock, since those belong to the person, not the look.
- All boot tests PASS; 170 / 0 on the default path, 0 FAIL on virtio.
- **Choice:** the RTC is taken as UTC (QEMU's default). On a machine whose
  RTC keeps local time, set `timezone = UTC`.
- Known gap: no NTP, so the clock drifts with the monotonic clock over
  very long uptimes.
- Screenshot: `build/progress-15-clock.png`.

## 18. Files

- `user/files`: a Finder-style browser of the root filesystem.
  - A Places sidebar (the root, shown as "CorrodeOS", and each top-level
    directory), a back button and the folder's name.
  - A list with folder and document icons and sizes, folders first.
  - A preview of the selected file: its name, its size, and its text on a
    sheet of paper, or "Not text" for binary files.
  - Translucent over the glass frame, resizable (zoom and edge drag lay it
    out again), and follows the theme live.
- Mouse: click a place, a folder (opens) or a file (preview); a second
  click on a selected entry opens it. Keyboard: Up/Down, Return opens,
  Backspace goes up.
- Read-only, through the same `app::FILES` blob capability the terminal
  holds. The launcher gives it only to the terminal and Files.
- The dock now has Files first (a white folder on blue), where Finder sits
  on macOS. Every dock-clicking test was re-aimed, since icons moved 28 px.
- `./corrode files-test` opens Files from the dock, then uses the keyboard
  to go into `/etc` and preview `desktop.conf`. PASS. All other boot
  tests PASS; 170 PASS / 0 FAIL.
- Known gap: no scrolling, so a folder with more entries than fit shows
  the first screenful.
- Screenshot: `build/progress-16-files.png`.

## 19. Mouse wheel and scrolling in Files

- The PS/2 driver now does the IntelliMouse knock (sample rates 200, 100,
  80, then GET_ID == 3). A mouse that answers sends four-byte packets with
  the wheel in the fourth byte. The log says "mouse up with a wheel"; a
  plain mouse still works as before. `corrode-input` decodes both packet
  sizes, with a host test.
- The kernel carries wheel notches with pointer events and sums them when
  moves coalesce. A new `window::SCROLL` event goes to the window *under*
  the pointer, not the focused one, as on macOS. `corrode-ui` exposes it
  as `Event::Scroll { notches, x, y }`.
- Files scrolls three lines a notch: the list or the text preview,
  whichever is under the pointer. Arrow keys keep the selection in view,
  and a resize keeps it in view too. A thin overlay scroller, macOS style,
  appears only when there is more than fits.
- files-test now turns the wheel three notches over the desktop.conf
  preview. The screenshot shows it nine lines down with the thumb moved.
  All boot tests PASS; 170 / 0, virtio 0 FAIL.
- Note for test writers: QEMU's monitor `mouse_move 0 0 1` is wheel
  *up*; use `-1` to scroll down.
- Known gap: the terminal has no scrollback yet, so the wheel does
  nothing there.
- Screenshot: `build/progress-17-scroll.png`.

## 20. Terminal scrollback

- The terminal already kept its last 500 lines; now you can see them.
  The wheel scrolls three lines a notch, and Shift+Page Up/Down page
  through them. Pressing any key other than a modifier jumps back to
  the prompt, as Terminal.app does.
- While scrolled back, a thin overlay scroller on the right shows where
  you are. New output keeps arriving underneath without yanking the view.
- `corrode-input` gained `Key::PAGE_UP` and `Key::PAGE_DOWN`.
- New `./corrode scrollback-test`: opens a terminal, runs `help` twice,
  and turns the wheel back four notches. Log "12 lines back", PASS; the
  screenshot shows older help text with the scroller. All other tests
  PASS; 170 / 0.
- Screenshot: `build/progress-18-scrollback.png`.
