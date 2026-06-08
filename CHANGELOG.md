# Changelog

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
