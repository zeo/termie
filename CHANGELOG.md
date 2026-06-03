# Changelog

## Unreleased (v0.x)

### Terminal fidelity
- **Kitty keyboard protocol** (disambiguate + report-event-types): `Shift+Enter` now inserts a newline in TUIs, with faithful modified-key reporting. termie advertises `TERM_PROGRAM=ghostty` so TUIs enable it; `Ctrl+J` and `\`-then-Enter remain as universal fallbacks.
- **OSC 8 hyperlinks** (Ctrl-click to open), **OSC 52** clipboard writes (read queries refused so a remote program can't exfiltrate the clipboard), and **OSC 4 / 10 / 11 / 12** color queries.
- **Underline styles** (single / double / curly / dotted / dashed), **strikethrough**, and **blink** are now rendered — previously parsed but never drawn.
- **Reflow on resize**: soft-wrapped lines rejoin and rewrap to the new width across scrollback and the live screen, preserving the cursor position.

### Workflow
- Splits and a new **"new tab here"** palette action open in the focused pane's directory (from OSC 7).
- **Per-tab shell** via "new tab: pwsh / cmd / wsl"; **WSL** added as a shell option.
- **Drag a file** into the terminal to type its path; **right-click** to copy the selection or paste.
- **Plugin dock widgets are clickable**, delivering a `widget_clicked` event to the owning plugin.

### Customization
- **`colors.conf`** color overrides and **`keybindings.conf`** rebindable keys under `%APPDATA%\termie\`.

### Fixes & hardening
- Fixed a **transposed PTY size** when a pane was promoted from the warm shell pool: the child process was told its width and height swapped, so programs that lay out to the reported width — diff-heavy TUIs especially — wrapped and padded their background bars to the wrong column count. Screen and PTY now resize through one helper so the two can't diverge.
- Fixed the title-bar **close button** being intercepted by the window resize border.
- Bounded the plugin JSON parser's recursion depth (stack-overflow guard); added a VT-parser stress test and other regression tests.

### Licensing
- Dual **MIT / Apache-2.0**, with bundled Maple Mono (OFL) and Nerd Fonts notices.

### Dev tooling
- A debug-only `--termview` (text dump) and `--png` (image) harness that render through the real parser/grid/atlas, for verifying terminal and rendering changes headlessly.
