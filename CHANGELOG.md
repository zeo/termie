# Changelog

## Unreleased

### Fixes
- **Copying soft-wrapped text no longer breaks it.** A long URL or command that wrapped across rows used to gain hard newlines at every wrap point when copied; the selection now copies the logical line unbroken (real newlines still break, and alt+drag block copies stay one line per row).
- **Apps asking for a steady cursor get one.** DECSCUSR's steady variants (`CSI 2/4/6 SP q` — vim's `guicursor`, many shells) were treated as their blinking siblings; the blink bit is now honored both ways, overriding the configured default until the app resets it.
- **`clear` can really clear now**: ED 3 (`ESC[3J`) erases the scrollback buffer as xterm defines, instead of only blanking the screen.
- **A crafted clipboard can't break out of a bracketed paste**: an embedded end-bracket sequence in pasted text is stripped so the remainder can't run as keystrokes.
- **Rounded box corners are round again.** TUIs drawing `╭─╮` frames (lazygit, gh-dash, many Rust and Go tools) were getting hard square corners: the GPU cell-filling path claimed the rounded-corner characters but can only draw rectangles. They now render through the quarter-arc rasterizer that was built for them, still filling the exact cell so borders connect at any line height.
- **The mouse wheel scrolls pagers now.** `less`, `man`, `git log`, and other full-screen apps without mouse reporting used to sit dead under the wheel (the alternate screen has no scrollback to scroll). The wheel now translates to arrow keys there — the "alternate scroll" behavior other terminals ship by default.
- **Slow touchpad scrolling works.** A gentle two-finger scroll delivers a few pixels per event, which rounded to zero lines and went nowhere; the fractional remainder now accumulates so precision touchpads scroll smoothly at any speed.
- **A tmux/neovim keyboard probe no longer corrupts colors.** The `CSI > 4;2 m` modifyOtherKeys negotiation was mis-read as SGR 4;2 and painted dim underlined text; sequences with a `>`/`?` marker are now kept out of the styling path.
- **Saving settings no longer deletes your `quake_key`.** The settings panel rewrites the config file, and the drop-down hotkey line was silently dropped from it every time — it now survives.
- **README had the command palette on the wrong shortcut** (`Ctrl+Shift+P`, which is pane mode). It's `Ctrl+P`, as the settings panel always said.
- **Scrollback stays put while output streams.** Scrolling up to read history and having a build (or an agent) print one more line no longer yanks you back to the bottom — the view now stays anchored to the exact text you were reading as new lines flow underneath. Typing or pasting still snaps you back to the live prompt, which is the behavior every other modern terminal settled on.
- **AltGr works on European layouts.** AltGr arrives as Ctrl+Alt on Windows, so typing `[` on a German layout (AltGr+8) was sent as `ESC [` — the start of an escape sequence — instead of a bracket, with the same corruption for `]`, `{`, `}`, `@`, `\`, `€` and every other AltGr character, in both the legacy and kitty keyboard encodings. Layout-translated text is now sent exactly as typed; a bare Ctrl+Alt chord keeps its escape encoding.

### Terminal fidelity
- **DECAWM autowrap mode** (`CSI ? 7 h/l`): programs that disable autowrap (`tput rmam`) to paint the last column without a spill now get the pinned-margin behavior instead of forced wrapping, and DECRQM reports the mode.
- **XTVERSION** (`CSI > 0 q`): termie now identifies itself (`termie <version>`) to programs that probe terminal identity the standard way, instead of staying silent.
- **XTWINOPS size reports** (`CSI 14t` / `16t` / `18t`): programs can now ask for the text area's pixel size, the cell size, and the cell count. Image tools (`imgcat`, `chafa`, `timg`, kitty's `icat`) size inline graphics from these — nothing can ioctl a pixel size through ConPTY, so without them termie's kitty-graphics support was hard for real tools to discover and scale to. The pixel reports answer with the renderer's true cell geometry (they stay silent rather than guess if it isn't known yet).
- **Kitty graphics cell-box sizing** (`c=` / `r=`): an image sent with a requested column/row box now draws scaled to that box instead of at its native pixel size — one axis alone keeps the aspect ratio, per the protocol. This is how `icat`-style tools fit a picture to your pane width.

### Rendering
- **Dim text (SGR 2) is legible now** — it was multiplied to roughly a fifth of its linear brightness and read as near-invisible; it now sits at the conventional "clearly dimmer, still readable" level.
- **The unfocused pane's cursor is a hollow block** instead of a translucent fill, so the character under it stays readable across a cockpit of panes — the same convention Windows Terminal and Ghostty use.

### Workflow
- **Prompt jump works out of the box.** `Ctrl+Up`/`Ctrl+Down` jump between prompts — the feature was fully built but nothing ever emitted the OSC 133 marks it listens for; the injected pwsh prompt hook now emits them. The hook also **wraps your own prompt instead of replacing it**: with profile loading on, starship / oh-my-posh keep working and still get cwd tracking and prompt marks.
- **Alt+drag selects a rectangle** (block/column selection, like Windows Terminal and WezTerm): grab a column of values out of `ls` output or a log without dragging whole lines along. Copy joins the rows with newlines and trims each row's trailing spaces. Inside a mouse-capturing TUI, add shift the way you would for any selection (`Shift+Alt+drag`).
- **Shift+click extends the selection** to the clicked cell instead of starting over — the anchor-extend every other terminal and editor does. Works with shift-drag, respects copy-on-select, and still lets shift bypass an app's mouse capture first.
- **A bell in a background tab now shows a dot on that tab.** Run an agent or a long build in another tab and the tab marks itself the moment its shell rings the bell (`BEL` — Claude Code and most CI-ish tools ring it when they finish or need input), so you can see *which* tab wants you instead of cycling through them. Viewing the tab clears the dot; while a tab is hovered the dot yields to the close button.
- **A bell while the window is unfocused flashes the taskbar button** (until you refocus, the standard Windows attention signal) — so an agent finishing in a minimized or covered termie still reaches you. Torn-off pane windows get the same treatment, scoped to their own taskbar button and tabs.
- **Program notifications ring through the same channel**: an iTerm2-style `OSC 9 ; message` or rxvt/tmux `OSC 777 ; notify` now counts as a bell — tab dot, taskbar flash, border flash — instead of being dropped. ConEmu's numeric `OSC 9` subcommands (progress, cwd) are still told apart and handled as before.
- **The notification's text shows on the status bar** for a few seconds — a bell-marked readout in the right cluster ("claude: waiting for your approval"), so you know *why* a tab is dotted before you switch to it. Torn-off windows show it on their own bar.

## 0.2.11-rc1 — 2026-06-25

### Resiliency
- **COM API resource cleanup**: Implemented an RAII `ComGuard` structure to balance successful COM initializations on the main GUI thread, preventing resource/reference count leaks when querying Explorer directories.
- **Robust plugin installation**: The directory move fallback now performs transactional cleanups. If the copy operation fails when installing a plugin across filesystem volumes, the partial folder is deleted to prevent a corrupted installation.

### Security
- **Plugin IPC message length limits**: Implemented a 256 KB message length limit on the reader thread for plugin stdout streams. Any line exceeding the limit is safely logged, discarded, and resynchronized, avoiding memory inflation or OOM crash vectors from rogue plugins.

### Diagnostics
- **Color override warnings**: Added console/log warnings in `colors.conf` parsing to report missing `=` signs or unparseable colors, making theme troubleshooting straightforward.

## 0.2.10 — 2026-06-18

### Fixes
- **Ctrl+C now copies the selection.** When text is selected, Ctrl+C copies it (and clears the selection) the way Windows Terminal does; with nothing selected it still passes through as the interrupt the shell expects. Ctrl+Shift+C remains the always-copy chord, and right-click now has a Copy entry too.

### Scrollback
- **The scrollbar is now grabbable.** The thin thumb on the right edge can be clicked and dragged to scroll through history, and clicking the track jumps to that spot. It also shows when you hover the right edge, so you can grab it from the live bottom — not only once you've already scrolled up.

## 0.2.9 — 2026-06-15

### Fixes
- **The tab bar no longer reads as a dark band above the content.** The bar was filled a shade darker than the terminal background, so against the lighter top of the window it looked like a dark stripe — and a full-screen app's first row (a banner, a logo) sitting right under it looked clipped or dimmed. The bar now matches the content background, divided by the same hairline trim as before.
- **Full-screen apps get a little breathing room at the top.** Content drawn on the very first row is no longer jammed against the bottom of the tab bar; the top inset now matches the sides and bottom.
- **No more garbled, stretched frame when restoring a minimized window.** Minimizing and then alt-tabbing back could briefly flash a huge, blurry, doubled image before the real screen appeared. termie no longer paints while minimized, so the last good frame is what shows on restore.
- **No leftover mouse-tracking after a full-screen app exits.** If a program left without cleanly turning mouse reporting back off, stray sequences such as `^[[<0;29;17M` could appear at the prompt when you moved or clicked. termie now drops mouse, application-cursor and extended-key modes when an app leaves the alternate screen.
- **The clickable-path underline is pinned to the real Ctrl key.** It now reads the physical key state, so a stale modifier can never leave the hover underline stuck on.

### Rendering
- **Block and box characters fill their cell completely.** Fractional blocks (`▁▂▃▄▅▆▇`, `▏▎▍▌▋▊▉`), the eighth-edges (`▔▕`) and the quadrant pieces (`▖▗▘▙▚▛▜▝▞▟`) now draw to the exact cell, so block-art and progress bars tile solid with no seams at any line height.

## 0.2.8 — 2026-06-14

### Packaging
- **The installer's Start-menu and desktop shortcuts now actually install.** The previous MSI registered them in a way that silently skipped them on a per-machine install, so the only shortcuts were whatever an older installer had left behind.
- **Installing now removes a leftover per-user install from older (pre-MSI) builds.** A copy from the old installer could sit earlier in `PATH` and own your taskbar/Start shortcuts, so launching termie kept running the old version even after the MSI installed the new one. The MSI now uninstalls that legacy copy as part of installing.

## 0.2.7 — 2026-06-14

### Fixes
- **No more stray underline under paths**: if the window lost focus while a modifier was held — most easily by Ctrl-clicking a path, which opens it and switches away — Ctrl could stay "held" internally, so afterwards hovering any path drew the clickable-link underline even though Ctrl wasn't pressed. termie now drops held modifiers and any hover-link highlight when the window loses focus.

## 0.2.6 — 2026-06-14

### Packaging
- **The installer is now an MSI** (`termie-<version>-windows-x64.msi`), built with WiX. It carries a stable upgrade code, so installing a new version **replaces** the old one instead of leaving a second copy behind — fixing the case where the previous installer could leave two installs side by side. Same integration as before: Start-menu and desktop shortcuts, `PATH`, the "Open in termie" right-click entry, and Add/Remove Programs. It now installs per-machine (one admin prompt). If you installed a build before 0.2.6, uninstall it once from Add/Remove Programs first.

## 0.2.5 — 2026-06-13

### Packaging
- **Releases now ship a Windows installer** (`termie-<version>-windows-x64-setup.exe`) instead of a zip — a wizard that installs termie, creates Start-menu and optional desktop shortcuts, adds it to your `PATH`, registers the "Open in termie" right-click entry, and provides a proper uninstaller (Add/Remove Programs). Built with Inno Setup.

## 0.2.4 — 2026-06-13

### Fixes
- **Box-drawing borders connect again**: lines, corners, tees and the block / shade elements are now drawn to fill the whole cell, so boxes no longer leave vertical seams at line heights above 1.0 (the font's own box glyphs are only ~1 em tall, which broke TUI frames like Claude Code's). Rounded corners stay rounded; double / dashed glyphs fall back to the font.
- **The exe icon is no longer generic**: termie.exe now embeds its application icon as a PE resource, so Explorer, the taskbar (pinned + grouped) and Alt-Tab show termie's icon — previously only a running window carried it.

## 0.2.3 — 2026-06-13

### Fixes
- **Address-bar launch lands in the folder again**: typing `termie` in File Explorer's address bar opens the first tab in that folder, not your home dir. The address bar starts a windows-subsystem app with no working directory of its own, so termie inherits Explorer's home dir and the process cwd is useless for this; it now recovers the folder from the Explorer window it was launched from. A shell-in-a-repo launch (real process cwd) is unchanged, and Start-menu / desktop / taskbar / Run-box launches — none of which have an Explorer window in the foreground — still restore the saved session.
- **A double-click no longer opens in termie's own folder**: double-clicking `termie.exe` sets the process cwd to the exe's folder, so a bare launch opened the first tab there. The exe's own directory is now treated as incidental — the same as the home dir — so a double-click behaves like a plain Start-menu launch (restore the saved session) instead of opening a tab inside the install folder.

## 0.2.2 — 2026-06-11

### Fixes
- **Release packaging** now runs under Windows PowerShell on the build runner (which has no PowerShell 7), so the zip is actually built and attached. 0.2.0 and 0.2.1 were tagged but their release builds didn't publish a binary; 0.2.2 is the first 0.2.x with an attached download.

## 0.2.1 — 2026-06-11

### Project
- **Plugins moved to the [`termie-plugins`](https://github.com/lintowe/termie-plugins) registry** — each plugin now lives there as source and is built into the catalog the in-app store fetches, and that's where plugins are contributed. No change to the termie binary or to how the store works.
- **CI hardening**: GitHub Actions are pinned to full commit SHAs, the workflow token is least-privilege (`contents: read`), and `cargo-audit` runs as part of CI.

## 0.2.0 — 2026-06-11

### Terminal fidelity
- **Taskbar progress** (ConEmu OSC 9;4): a program reporting progress — winget, CI scripts, build tools — now lights up termie's Windows taskbar button: green for normal progress, red for error, yellow for paused, pulsing for indeterminate. Progress from every pane in the window is folded into one value (error wins, then paused, then the largest percentage), clears when the reporting pane closes or resets, and keeps updating while the window is minimized — which is exactly when the taskbar is what you're watching.

### Workflow
- **Launch into the current folder**: typing `termie` in File Explorer's address bar (or the Run box, or from a shell sitting in a repo) now opens the first tab in that directory, the way `cmd` does — Windows Terminal lands in your home dir unless you pass `-d .`. A plain Start-menu / desktop launch (its working dir is your home dir) still restores the saved session; a folder or `--cwd` launch is an ad-hoc window and won't overwrite that session.
- **`Ctrl`+mouse-wheel font zoom**, matching Windows Terminal; the palette's font increase / decrease / reset remain.
- **Borderless fullscreen** on `F11` (also "toggle fullscreen" on the palette, rebindable or unbindable via `keybindings.conf` like everything else).
- The classic conhost chords — **`Ctrl+Insert` copy, `Shift+Insert` paste** — now ship as defaults, and `keybindings.conf` understands `insert` / `delete` / `home` / `end` / `pageup` / `pagedown` (plus `ins`/`del`/`pgup`/`pgdn` aliases) as combo keys.

### Performance
- **No GPU frames while minimized**: terminal output streaming into a minimized (or quake-hidden) window still updates the grid and the taskbar progress but skips painting; the first turn after restore paints the latest state.

### Hardening
- **Pasting can't freeze the window anymore**: PTY input now goes through a per-pane writer thread, so a child that has stopped reading (a paused pager, a stopped process) fills the ConPTY pipe without taking the UI thread down with it. Input ordering is unchanged.
- **Clipboard open is retried** (~15 ms exponential backoff) before giving up — a clipboard manager or another app briefly holding the clipboard no longer silently eats a copy or paste.

### Plugins
- **Tier-2 widget drawing** (plugin protocol `api_version` 2): a plugin can now send an immediate-mode `draw` list — `rect` and `text` primitives in coordinates normalized to the widget canvas, colored by palette role or `#hex` — painted in a `canvas_h`-tall box under the widget title. Each primitive is clipped to the widget so a plugin can never paint over the terminal, and the list is bounded (256 primitives, coordinates clamped). Tier-1 text widgets are unchanged, and a v1 plugin that never sends a draw list is unaffected. The reference tamagotchi plugin upgrades to graphical food/joy meters on a v2 host and falls back to text bars on an older one.
- **AppContainer sandboxing** (opt-in): set `plugin_sandbox=appcontainer` in `config` to run every plugin inside a Windows AppContainer — low integrity, with no access to your files, registry, network, windows, or other processes unless granted. A plugin's `network` permission maps to the internetClient capability, and the plugin's install directory is granted read+execute so its executable loads. Off by default; on a sandbox-launch failure the plugin is skipped rather than run unconfined.
- The in-app plugin **store is now mouse-clickable** — click a card to select it, its INSTALL / ENABLED / DISABLED chip to act on it, or the × to close. Keyboard navigation (arrows, enter, `r`, esc) still works.
- Non-sandboxed plugins no longer flash a **console window** on launch — the host spawns them with `CREATE_NO_WINDOW`, matching the sandbox path — so a console-subsystem plugin like the reference tamagotchi stays windowless.
- The reference **tamagotchi** pet is now animated and interactive: a pixel creature that idles, blinks, hops, and naps, startles into a sparkly bounce on a `bell`, perks up on focus changes, and pets/feeds itself when you click its card — with food and joy shown as segmented gauges. It only emits a frame when the drawing actually changes, so an idle pet lets the terminal idle too (now declares `api_version` 2).
- Both reference plugins (**tamagotchi**, **relay**) are built as Windows GUI-subsystem binaries, so they never flash a console window even on an older host that predates the `CREATE_NO_WINDOW` spawn fix.
- The plugin **marketplace** no longer flashes a console window while browsing: the `gh` / `curl` / `tar` helpers it shells out to (and the AppContainer `icacls` grant) are spawned with `CREATE_NO_WINDOW`.

### Rendering
- **Lower input latency by default**: terminal output now paints inline instead of through the request-redraw hop, shaving up to a frame of input→photon latency while staying tear-free under Fifo vsync and one present per loop turn. Set `inline_paint=false` to restore the previous path. An optional latency HUD (`latency_hud=true`) draws a live input→photon (p50/p95) and frame-interval readout in the bottom-left for measuring it.

### Fixes
- The settings panel now **eases shut** instead of snapping. The close tween was front-loaded by an ease-out curve, so most of the motion happened in the first few frames and it read as instant; it now eases in-out (and runs a touch longer) so it visibly slides back to the terminal.
- The **browse** button in the settings PLUGINS row is no longer clipped along its top edge — the scrollable body now has a few px of top headroom so the first row clears the scissor at `body_top`.

## 0.1.3 — 2026-06-08

### Terminal fidelity
- **Kitty PNG images** (`f=100`): the graphics protocol now decodes PNG payloads (paletted / grayscale / 16-bit are normalized to RGBA8), alongside the existing raw RGB (`f=24`) and RGBA (`f=32`).

### Dev tooling
- Live PTY integration tests — spawn a real shell, feed its output through the terminal (answering the queries ConPTY gates on), and assert the rendered grid. `#[ignore]`d so they stay out of CI; run with `cargo test -- --ignored`.

## 0.1.2 — 2026-06-08

### Plugins
- Rebuilt the in-app plugin store as a **full-page view** — each plugin is a card with its name, version, description, permission badges, and an **Install** / **Enabled** / **Disabled** action, with live installed/available counts in the header and distinct loading, empty, and unreachable-catalog states.
- Catalog and plugin downloads authenticate through the GitHub CLI (`gh`) when the file lives in the catalog repo, so a private catalog works with your existing login; anonymous `curl` stays the fallback for a public host.
- The store tells a failed catalog fetch apart from an empty catalog and shows an accurate message instead of always blaming "offline".

### Fixes
- Settings: the PLUGINS section rule no longer draws through the "browse" button.

## 0.1.1 — 2026-06-08

### Terminal fidelity
- **Kitty keyboard protocol** (disambiguate + report-event-types): `Shift+Enter` now inserts a newline in TUIs that support it, with faithful modified-key reporting. termie advertises `TERM_PROGRAM=ghostty` so those apps enable it; `Ctrl+J` and `\`-then-Enter remain as universal fallbacks.
- **OSC 8 hyperlinks** (Ctrl-click to open), **OSC 52** clipboard writes (read queries refused so a remote program can't exfiltrate the clipboard), and **OSC 4 / 10 / 11 / 12** color queries.
- **Underline styles** (single / double / curly / dotted / dashed), **strikethrough**, and **blink** are now rendered — previously parsed but never drawn.
- **Reflow on resize**: soft-wrapped lines rejoin and rewrap to the new width across scrollback and the live screen, preserving the cursor position.
- **Inline images** via the kitty graphics protocol — raw RGB (`f=24`) and RGBA (`f=32`), chunked transmission, image IDs, query, and delete/delete-all; decoded images are scroll-anchored to the cell grid, acked, and packed into a GPU RGBA atlas. (PNG `f=100` is a deferred fast-follow.)
- **Color emoji** now render in full color through the RGBA atlas, and **combining marks / ZWJ / variation selectors** fold into per-cell grapheme clusters so width, rendering, and copy all keep the whole grapheme.
- **More escapes**: DECOM origin mode, DECRQM/DECRPM mode reports, DA2, REP, DECSTR soft reset, DEC special-graphics charsets (SO/SI), and shift/alt/ctrl modifiers reported in mouse events.

### Workflow
- Splits and a new **"new tab here"** palette action open in the focused pane's directory (from OSC 7).
- **Per-tab shell** via "new tab: pwsh / cmd / wsl"; **WSL** added as a shell option.
- **Drag a file** into the terminal to type its path; **right-click** to copy the selection or paste.
- **Plugin dock widgets are clickable**, delivering a `widget_clicked` event to the owning plugin.
- **Quake drop-down mode** (opt-in): set `quake_key=ctrl+grave` in `config` to register a process-global hotkey that drops termie down from the top of the screen (always-on-top) and hides it again. Also on the command palette as "quake drop-down".
- **Tear a pane off into its own window**; torn-off windows are full peers — their own tabs, splits, mouse, focus, IME, and OSC handling.
- **Session restore**: the tab and split layout persists and is restored on launch, with crash recovery.
- **tmux-style pane zoom**, **tab rename** from the palette (persisted across restarts), and **keyboard pane resize** (`Shift`+arrows) in pane mode.

### Customization
- **`colors.conf`** color overrides and **`keybindings.conf`** rebindable keys under `%APPDATA%\termie\`.

### Fixes & hardening
- Fixed a **transposed PTY size** when a pane was promoted from the warm shell pool: the child process was told its width and height swapped, so programs that lay out to the reported width — diff-heavy TUIs especially — wrapped and padded their background bars to the wrong column count. Screen and PTY now resize through one helper so the two can't diverge.
- Fixed the title-bar **close button** being intercepted by the window resize border.
- Bounded the plugin JSON parser's recursion depth (stack-overflow guard); added a VT-parser stress test and other regression tests.

### Performance
- **Allocation-free hot paths**: the PTY scanner reuses its buffers and bulk-copies the runs between escapes; SGR parsing, combining-mark interning, and the status bar avoid per-frame allocation; a hand-rolled FxHash backs the glyph and cluster caches (−33% on the full-screen grid draw, +25% plain-parse throughput).
- **Faster first prompt**: the warm-shell pool starts before the first paint, the glyph atlas builds on a worker thread overlapping GPU init, printable ASCII is pre-warmed, and the blocking system-font scan is deferred until the prompt is on screen.
- **Lower input latency**: PTY output is coalesced to one paint per frame, the renderer queues a frame ahead, and the release build links with fat LTO.

### Licensing
- Dual **MIT / Apache-2.0**, with bundled Maple Mono (OFL) and Nerd Fonts notices.

### Dev tooling
- A debug-only `--termview` (text dump) and `--png` (image) harness that render through the real parser/grid/atlas, for verifying terminal and rendering changes headlessly.
- A `cargo test golden` snapshot suite plus a deterministic parser property-fuzz (seeded adversarial byte streams + random reflows, asserting no panic and grid self-consistency) that guard terminal and render correctness headlessly.
- A private, feature-gated `--microbench` harness (absent from the shipped binary) that times the CPU hot paths — scanner, parser, grid, atlas, draw, and image decode.
