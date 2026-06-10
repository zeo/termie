# Changelog

## Unreleased

### Terminal fidelity
- **Taskbar progress** (ConEmu OSC 9;4): a program reporting progress — winget, TUIs, CI scripts — now lights up termie's Windows taskbar button: green for normal progress, red for error, yellow for paused, pulsing for indeterminate. Progress from every pane in the window is folded into one value (error wins, then paused, then the largest percentage), clears when the reporting pane closes or resets, and keeps updating while the window is minimized — which is exactly when the taskbar is what you're watching.

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

## v0.1.3 — 2026-06-08

### Terminal fidelity
- **Kitty PNG images** (`f=100`): the graphics protocol now decodes PNG payloads (paletted / grayscale / 16-bit are normalized to RGBA8), alongside the existing raw RGB (`f=24`) and RGBA (`f=32`).

### Dev tooling
- Live PTY integration tests — spawn a real shell, feed its output through the terminal (answering the queries ConPTY gates on), and assert the rendered grid. `#[ignore]`d so they stay out of CI; run with `cargo test -- --ignored`.

## v0.1.2 — 2026-06-08

### Plugins
- Rebuilt the in-app plugin store as a **full-page view** — each plugin is a card with its name, version, description, permission badges, and an **Install** / **Enabled** / **Disabled** action, with live installed/available counts in the header and distinct loading, empty, and unreachable-catalog states.
- Catalog and plugin downloads authenticate through the GitHub CLI (`gh`) when the file lives in the catalog repo, so a private catalog works with your existing login; anonymous `curl` stays the fallback for a public host.
- The store tells a failed catalog fetch apart from an empty catalog and shows an accurate message instead of always blaming "offline".

### Fixes
- Settings: the PLUGINS section rule no longer draws through the "browse" button.

## v0.1.1 — 2026-06-08

### Terminal fidelity
- **Kitty keyboard protocol** (disambiguate + report-event-types): `Shift+Enter` now inserts a newline in TUIs, with faithful modified-key reporting. termie advertises `TERM_PROGRAM=ghostty` so TUIs enable it; `Ctrl+J` and `\`-then-Enter remain as universal fallbacks.
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
