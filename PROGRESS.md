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

## 21. Alt+Tab window switcher

- `kernel/src/switcher.rs`: macOS's Cmd+Tab panel. Alt+Tab puts a glass
  panel in the middle of the screen with an icon per window, most recently
  in front first. The front window always leads and minimized windows come
  last. The next window is picked, with a soft highlight and its title
  underneath.
  - Tab (Alt held) moves along with a spring and Shift+Tab goes back.
  - Releasing Alt brings the pick forward, restoring it if minimized.
  - Escape cancels.
  - A window closing while the panel is up puts it away.
- Icons are the dock's own stencils (new `Dock::stencils`); the label is
  the window's own title stencil. The only new stencil is the rounded
  highlight, at atlas row 408 in the right half.
- With a single window, Alt+Tab still brings it forward with no panel.
  The old cycle-by-index shortcut is gone.
- New `./corrode switcher-test`: three windows from the dock. A quick
  Alt+Tab switches to Terminal ("switched to window 2"), then a held one
  is screenshotted mid-hold. PASS, and on virtio too. shortcut-test still
  PASSes; it needed minimized-last ordering to bring the minimized window
  back. All tests PASS; 170 / 0.
- **QEMU quirk:** after `sendkey KEYS HOLD_MS`, QEMU delivers no more keys
  and drops the held keys' release. The held Alt+Tab therefore comes last
  in the test, and the switch itself is checked with a plain tap.
- Screenshot: `build/progress-19-switcher.png`.

## 22. Resize from every edge

- Resizable windows can now be resized from all four edges and corners.
  - The left edge: 8 px into the window body.
  - The top edge: the title bar's top 4 px. The rest of the bar still
    drags the window, and the traffic lights are below that strip.
  - The bar's top corners act as diagonal handles.
- Dragging the left or top edge moves the window so that its right and
  bottom edges stay put. Edge drags still respect the minimum size
  (240x120) and the screen edges.
- resize-test gained a second drag: the top-left corner 60x40 bigger
  ("is now 860x480"). The screenshot shows the window grown up and left,
  with its bottom-right corner exactly where it was. PASS; all other tests
  PASS; 170 / 0.
- Note for test writers: one very large `mouse_move` gets clamped on the
  way in (one PS/2 packet holds about ±255). Split long moves into steps.
- Known gaps:
  - There is no resize cursor shape yet, since the pointer has one
    stencil.
  - Edge resizing remains software-renderer only; virtio can't show a
    surface at a new size yet.
- Screenshot: `build/progress-20-edges.png`.

## 23. Resize cursors

- Over a resizable window's edge, the pointer becomes a double-headed
  arrow: ↔ on the left and right edges, ↕ on the top and bottom, ⤡ and ⤢
  on the corners. The arrow is held through the whole drag, and dragging
  a title bar keeps the normal arrow.
- Four new stencils (outline and fill each), 21 px square, sit in the
  pointer's atlas row from x 96. They are sampled four by four like the
  arrow. The hot spot is in the middle. If they can't be uploaded, the
  plain arrow simply stays.
- Edge detection is now one function, `Desk::edges`, shared by the hover
  cursor and the press that picks up an edge. They can't disagree about
  where an edge is.
- New `./corrode cursor-test` hovers a terminal's right edge, then its
  bottom-right corner. Log "pointer is now Across", then "Falling". PASS.
  `build/progress-21-cursor.png` is a 4x crop of the corner cursor.
  - Under GPU=virtio it reports FAIL by design: resizing is software-only,
    so no edge exists and the arrow stays.
- All other tests PASS; 170 / 0.

## 24. Search (Alt+Space)

- `kernel/src/spotlight.rs`: Spotlight and KRunner in one. Alt+Space puts
  a glass field near the top of the screen, with a magnifier and a faint
  "Search" placeholder. Typing narrows the dock's programs to those whose
  names contain it, names that start with it first. Each result shows the
  dock's icon and name, and the pick gets an accent highlight.
  - Up and Down move the pick.
  - Return opens the pick, or brings its window forward if it's already
    running, as Spotlight does.
  - Escape or Alt+Space again closes it.
- While search is up it has the keyboard; keys don't reach windows.
- Atlas: the query line (400x28) and the magnifier sit in the right half
  at row 408, past the switcher's highlight. Icons and names reuse the
  dock's stencils (new `Dock::apps`/`Dock::label`).
- New `./corrode search-test`: Alt+Space, type "s", Return. Settings is
  listed first (starts with s) above Files (contains s), and Settings
  opens. PASS on the default and virtio displays. All other tests PASS;
  170 / 0.
- README shortcuts updated.
- Known gaps:
  - Only programs are searched, not files.
  - With `dock = false` there is nothing to search, and the log says so.
  - The pick highlight is a plain rectangle; the atlas has no room left
    for a wide rounded stencil.
- Screenshot: `build/progress-22-search.png`.

## 25. Search finds files

- Once something is typed, Alt+Space search also lists files from the root
  filesystem whose names contain it, after the matching programs. Names
  that start with the query come first, and at most four files are shown.
  Each row has a document icon and reads like "motd   /etc": the name,
  then the folder.
- Return on a file shows it in Files: the folder opens and the file is
  picked, so its preview shows.
  - If Files is open, its window gets a new `window::REVEAL` event (the
    path packed into the four words, at most 32 bytes) and comes forward.
  - If not, Files is started from the dock, and the event is sent as its
    window opens.
- `corrode-theme` gained `tar_files`, an allocation-free iterator over a
  ustar archive's files; `find_in_tar` is now built on it. Host-tested.
- `corrode-ui` has `Event::Reveal`; Files has `Browser::reveal`.
- New `./corrode reveal-test`: Alt+Space, type "mot", Return. Files logs
  "showing /etc/motd". PASS, on virtio too. search-test still PASSes and
  its screenshot now shows two apps and four files. All tests PASS;
  170 / 0.
- Known gaps:
  - Paths longer than 32 bytes can't be revealed; the log says so.
  - Files are matched by name only, not by contents.
- Screenshots: `build/progress-23-search-files.png`,
  `build/progress-24-reveal.png`.

## 26. Notifications

- Programs can now post notifications. `corrode_ui::notify(text)` sends
  `window::NOTIFY` (up to 32 bytes of ASCII in the four words) to the
  window server. The server files it in `kernel/src/notify.rs` under the
  sender's window title, found by the task that sent it. At most 8 wait;
  the oldest is dropped past that.
- `kernel/src/banner.rs`: macOS-style banners.
  - A glass pane 340x66 at the top right, under the menu bar, with the
    sender's dock icon, its name in bold and the text below.
  - It springs in from beyond the right edge.
  - It eases out after four seconds, or at once when clicked; the click
    goes no further.
  - One shows at a time, and the next waiting one follows.
- The text uses two of the window title slots, borrowed through a new
  `Desk::draw_text` (draw_title is built on it) and given back when the
  banner goes. If no slots are free, the log says so and nothing is shown.
- The terminal gained `notify TEXT`.
- New `./corrode notify-test`: two notifications. The first times out by
  itself, and the second (`build/notify.png`) is clicked away. PASS, on
  virtio too. All tests PASS; 170 / 0.
- Known gaps:
  - No Notification Center listing past notifications.
  - Text longer than its slot (256 px) is cut.
- Screenshot: `build/progress-25-notify.png`.

## 27. Control Center

- New window kind: `window::POPOVER` (flag 8).
  - No title bar, lights or resizing.
  - Placed under the right-hand end of the menu bar and drops in from it.
  - The desk sends it CLOSE when it loses focus, or when anything outside
    it is clicked.
  - A click on the menu bar that closes a popover goes no further, so
    clicking the clock again shuts Control Center instead of reopening it.
  - Popovers are left out of the switcher, Alt+` cycling and the menu
    bar's front-app name.
- New program `user/control` (Control Center), a glass popover:
  - six tiles: Dark Mode, Glass, Dock, Shadows, Motion, and "Settings..."
    (asks the launcher for Settings, then closes);
  - the eight accent swatches below the tiles.
  - Each toggle goes through `set_theme`, the same as Settings, so every
    window follows.
- The panel opens it when clicked within 170 px of the right edge (the
  clock), via a new `launcher::CONTROL` (5).
- Choice: the launcher now holds a send capability to itself
  (LAUNCHER_SELF_SLOT 6), handed only to Control Center so it can ask for
  Settings. Only programs that get it can start other programs.
- New `./corrode control-test`: opens it from the clock, turns Dark Mode
  off, then clicks the clock to close it. PASS, on virtio too. All other
  tests PASS; 170 / 0.
- Known gaps:
  - Glass only toggles Off/Regular; the Clear mode stays in Settings.
  - No keyboard control inside the popover.
- Screenshot: `build/progress-26-control.png`.

## 28. Lock screen

- New window kind: `window::COVER` (flag 16).
  - It takes the whole screen, with no chrome and no shadow, and fades in.
  - The desk raises it above everything except the pointer (menu bar,
    dock, banners, switcher and search included).
  - While it is up, every key goes to it, so Alt+Tab, Alt+Space and the
    other shortcuts do nothing.
  - Covering the screen puts away any open search or switcher.
  - Covers get the CLOCK anchor, as docked windows do.
- New program `user/lock`, the lock screen:
  - a clear surface over a full-screen glass pane, so it is the blurred
    desktop;
  - the date, the time at 112 px bold, an avatar circle in the accent
    colour with "CorrodeOS" under it, and a "Press Return to unlock" pill;
  - it updates every minute.
  - Return or clicking the pill unlocks it (the program exits).
- Alt+L locks (new `Shortcut::Lock`, `launcher::LOCK` = 6). The launcher
  starts only one lock screen at a time (`windows::locked()`, which skips
  windows whose program has exited).
- Control Center is now 4x2 tiles: a 24-Hour clock toggle and a Lock tile
  were added.
- Choice: no password, since there are no accounts or a keyring to check
  one against. The README says it hides the screen rather than securing
  it.
- New `./corrode lock-test`: locks with Alt+L, checks that Alt+Tab and
  Alt+Space do nothing under it, unlocks with Return, then locks again and
  unlocks with a click. PASS, on virtio too. All other tests PASS;
  170 / 0.
- Known gaps:
  - No unlock animation; it just vanishes.
  - The dock still notices the pointer under the cover (unseen).
- Screenshots: `build/progress-27-lock.png`,
  `build/progress-27-control.png`.

## 29. Windows fade out; the dock rests under a cover

- Closed windows fade out over 180 ms (scaled by the theme's motion speed;
  instant with motion off) instead of vanishing.
  - Their layers move to a `fading` list, and the surface and title slot
    are only given back once the fade ends, so nothing new is drawn into
    a slot still on screen.
  - Docked windows still go at once.
  - A fading cover stays above the menu bar and dock, so the lock screen
    dissolves into the desktop instead of sinking under them.
  - Choice: `Ease::Out` (fast start). With `Ease::In`, a 180 ms fade looks
    like no fade at all for its first half.
- Checked with a temporary 2 s fade: `build/progress-28-fade.png` is the
  lock screen part-way through dissolving (the build ships 180 ms).
  lock-test also takes `build/lock-fade.png` just after Return.
- The dock no longer lifts or labels icons for a pointer moving under the
  lock screen.
- Tests: all PASS (170 / 0). minimize-test failed once and passed on a
  rerun (a timing flake when the terminal starts slowly).
- Found, not new: `GPU=virtio ./corrode churn-test` closes only 13 of 20.
  It does the same without this change: under virtio the terminal can
  take longer than the test's 1.2 s to open, so some `exit` keystrokes
  reach the desktop instead. Default-GPU churn-test PASSes.

## 30. virtio: closed windows give their surfaces back

- The churn-test failure under `GPU=virtio` (section 29) was not timing.
  After 13 windows, `terminal: no window: NoSpace`. The cause:
  - the remote renderer's `forget_surface` always said no, so under
    virtio no closed window's surface was ever retired;
  - the driver also kept every surface capability in a new slot, for
    good.
- Fix:
  - New `gpu::FORGET` (7): the kernel asks the driver to detach the
    surface and drop its capability, then retires the surface as the
    software renderer path always did.
  - The driver tracks which slot holds which surface (`Held`, as many as
    the renderer shows) and reuses free slots.
  - A retired surface keeps its frames and its fixed address, so when it
    is handed out again the driver maps the same pages afresh.
- `GPU=virtio ./corrode churn-test`: 20 opened, 20 closed, PASS (it was
  13). Every test PASSes on the default GPU (170 / 0). Under virtio,
  input-test gives 177 / 0, and everything passes except the known
  resize-only gaps: resize-test, and minimize-test's zoom-size check
  (sections 13 and 22).

## 31. Resizing under virtio

- Edge dragging and zoom-to-fill now work under `GPU=virtio` too; the
  gap noted in sections 13, 22 and 23 is closed.
- How: a resized surface is sent to the driver again with
  `gpu::SURFACE`, as the software path already did.
  - A surface keeps its pages and its address, so the driver's
    `map_surface` maps it afresh at the new size.
  - The driver now drops the older capability for that surface, so
    repeated resizes don't use up slots.
  - The kernel calls `desk.allow_resize(true)` for both renderers.
- Tests: the whole suite (17 boot tests) PASSes on both the default GPU
  and virtio. The first time all of them pass on both.
  input-test: 170 / 0 default, 177 / 0 virtio.
- Screenshot: `build/progress-30-virtio-resize.png` (virtio, after the
  top-left drag to 860x480).

## 32. Window snapping

- Drag a window by its bar to an edge of the screen:
  - left (pointer within 2 px of it) → the left half;
  - right → the right half;
  - top → fill.
  - The space is the same as zoom's: under the menu bar, above the dock
    (or 8 px from the bottom with the dock off), 8 px margins and an 8 px
    gap between halves.
- While a window is held over a zone, a glass pane at about two-thirds
  opacity springs out of the pointer to the target rectangle, behind the
  window, as Plasma's does. Moving from zone to zone retargets it;
  leaving every zone removes it.
- Letting go resizes and moves the window there (`desktop: window N
  snapped Left`). Zoom then puts it back where it was picked up, at its
  old size.
- Only resizable windows snap; Settings and About just move as before.
- Choice: no persistence of settings across boots for now. There is no
  block driver, and writing a disk or CMOS NVRAM from this kernel on real
  hardware could damage the machine's disks or firmware settings. That is
  too close to "don't touch the boot setup" to do unattended.
- New `./corrode snap-test`: drags a terminal to the left half (preview
  in `build/snap.png`), then to the right half (`build/snapped.png`).
  The full suite, 18 boot tests, PASSes on both GPUs; input-test is
  170 / 0 default, 177 / 0 virtio.
- Known gap: dragging a snapped window off its half keeps the snapped
  size, where macOS would restore the old size as it is pulled away.
- Screenshots: `build/progress-31-snap-preview.png`,
  `build/progress-31-snapped.png`.

## 33. Dragging a snapped window away restores its size

- Closes the gap from section 32. Picking up a window that is snapped or
  zoomed (it has an `unzoomed` frame saved) now gives it back its earlier
  size on the first move of the drag (`desktop: window N unsnapped to
  600x340`), as macOS does.
  - It stays held at the same fraction of its bar's width, so the pointer
    stays on the bar where it was grabbed.
  - The saved position is forgotten, since the drag decides that now.
- This also applies to zoomed (filled) windows dragged by their bar.
  Choice: the same rule as snapped ones, because both are "a window
  filling space it was put in".
- snap-test now also drags the right-snapped window into the middle and
  checks that it ends at 600x340 (`build/unsnapped.png`). Full suite
  PASSes on both GPUs (19 checks each); input-test is 170 / 0 default,
  177 / 0 virtio.
- Screenshot: `build/progress-32-unsnapped.png`.

## 34. Recent notifications in Control Center

- The kernel keeps the last 5 notifications with when they came
  (`notify::recent`), whether or not their banner was ever shown.
- New window-server call `window::HISTORY` (74). Word 0 picks which
  (0 = newest), word 1 picks the part:
  - part 0 answers the count, the sender's 8-byte title and the age in
    seconds;
  - part 1 answers the 32-byte text.
- `corrode_ui::notifications()` returns them as `Note { from, text, age }`.
- Control Center fetches them when it opens and adds a Notifications card
  under the accent swatches:
  - an accent dot, the sender in bold and "now / 5m ago / 2h ago" on the
    right, with the text below;
  - up to 3 rows, or "Nothing new". The popover's height follows.
- Banners and popovers no longer overlap:
  - opening a popover dismisses any banner on screen, as opening
    Notification Center does on macOS;
  - new banners wait in the queue until the popover is gone.
- New `./corrode history-test`: two `notify` commands from the terminal,
  then the clock; it checks for "2 recent notifications"
  (`build/history.png`). Full suite PASSes on both GPUs (20 checks each);
  input-test is 170 / 0 default, 177 / 0 virtio.
- Known gaps:
  - Control Center reads the list once, when it opens, so it doesn't
    update while open.
  - There is no way to clear the list.
- Screenshot: `build/progress-33-history.png`.

## 35. Control Center from the keyboard

- Closes the keyboard gap from section 27. Control Center has the
  keyboard when it opens, and:
  - the arrows move a highlight over the 8 tiles (a 2-column grid) and
    the 8 swatches below. Down from the last row of tiles goes to the
    swatches under that column, and Up from a swatch goes back to the
    tile above its half of the row;
  - Return or Space acts on the highlighted item, through the same
    `click()` the mouse uses (at its centre), so they cannot disagree;
  - Escape closes it.
- The highlight only appears once a key is pressed, as macOS shows focus
  only to keyboard users.
- How it looks: an accent rounded rectangle 3 px larger than the item,
  under it. The tiles are translucent, so it reads as an accent ring
  round an accent-tinted tile.
  - Choice: I meant to clear the inside for a plain ring, but the ui's
    `rounded` blends rather than replacing, so CLEAR does nothing. The
    tinted look works as a highlight, so I kept it.
- New `./corrode keys-test`: Down ×3 and Right from the clock-opened
  popover land on 24-Hour (`build/keys.png`), Return flips it, Escape
  closes. Full suite PASSes on both GPUs (21 checks each); input-test is
  170 / 0 default, 177 / 0 virtio.
- Screenshot: `build/progress-34-keys.png`.

## 36. Desktop right-click menu

- Right-click the bare desktop (not a window, the dock or a banner):
  the desk notes where, and asks the launcher for the new `menu` program
  (`launcher::MENU` = 7).
- `user/menu` opens a glass popover with the new `window::MENU` flag
  (32). The desk places its corner at the right-click and keeps it on
  screen.
- Items:
  - New Terminal and Open Files;
  - Next Wallpaper (cycles Aurora → Ribbons → Dunes → Gradient);
  - Light/Dark Appearance (the label follows the theme);
  - Settings..., Lock Screen and About CorrodeOS.
  - Rules separate the groups. Picking an item does it and closes the
    menu.
- Hover: new `window::HOVER` (75) / `Event::Hover`. The desk sends a
  popover the pointer's position on it, only when that changes and only
  to popovers, plus (-1, -1) when the pointer leaves. The menu lights
  the item under the pointer in the accent with white text, as macOS
  menus do.
- Keyboard: Up/Down (skipping rules, wrapping round), Return and Escape.
- Any button pressed outside a popover now closes it (it was the left
  only), so a second right-click replaces the menu.
- Found while testing: the wallpaper is a layer too, so "nothing under
  the pointer" never happens. The new `Desk::on_desktop` checks for no
  window, no dock and no banner.
- The menu holds the launcher capability, as Control Center does.
- New `./corrode menu-test`: right-click, hover Next Wallpaper
  (`build/menu.png`), click it (the wallpaper becomes Ribbons), then
  right-click and click outside to close. Full suite PASSes on both GPUs
  (22 checks each); input-test is 170 / 0 default, 177 / 0 virtio.
- Screenshots: `build/progress-35-menu.png`,
  `build/progress-35-ribbons.png`.

## 37. Dock icon menus

- Right-click a dock icon: the menu program opens above it, centred on
  the icon, with:
  - the app's name in bold (not pickable);
  - New Window;
  - Quit <app>, only if it is running.
- How the menu knows what it is for: the desk records what was
  right-clicked in `kernel/src/context.rs`: the kind (desktop or dock),
  the launcher program, whether it runs, and the title. The menu asks
  with the new `window::CONTEXT` (76) / `corrode_ui::context()` and
  builds its items at run time (a `Vec<Item>` now, not a const list).
- Quit: the new `window::QUIT` (77) / `corrode_ui::quit(title)`. The
  window server sends CLOSE to every live, undocked window with that
  title (`windows::close_titled`) and answers how many.
- The dock's hover label is taken down when a context menu opens and
  stays down while one is open. Before this fix it sat over the menu's
  bottom item.
- New dock accessors: `at`, `program`, `is_running`.
- New `./corrode dock-menu-test`: a terminal from the dock and one from
  the menu bar button, then right-click the Terminal icon
  (`build/dock-menu.png`) and pick Quit Terminal; "quit asked 2
  window(s)". Full suite PASSes on both GPUs (23 checks each);
  input-test is 170 / 0 default, 177 / 0 virtio.
- Known gap: no "Show All Windows" or list of open windows in the menu,
  as macOS has.
- Screenshot: `build/progress-36-dock-menu.png`.
