# Changelog

## Unreleased

### Security
- **Image memory is bounded as a whole, not just per image.** The kitty/sixel stores capped each image at 64 MB and kept 32 of them — which let one hostile stream pin about 2 GB. A 256 MB total budget now evicts oldest-first, and concurrent chunked transfers are capped at 4 reassembly buffers so interleaved never-finished uploads can't hold 64 MB each across unbounded ids.
- **`cargo audit` is clean of vulnerabilities.** Bumped `anyhow` (RUSTSEC-2026-0190) and `memmap2` (RUSTSEC-2026-0186), pulled `zbus_xml` past the vulnerable `quick-xml` 0.39 line, and patched `wayland-scanner` to the upstream commit that uses `quick-xml` 0.41 (RUSTSEC-2026-0194/0195) until crates.io ships it. The remaining audit note is the informational `ttf-parser` unmaintained warning via `cosmic-text`/`fontdb`.

### Installer
- **Old MSI leftovers can't survive a native install.** Removing the previous per-machine MSI now elevates via UAC when a quiet uninstall is not enough, then scrubs `Program Files\termie`, the all-users Start shortcut, and a machine PATH entry so Search never shows two termies side by side.

### Interface
- **Drop a folder on the tab strip to open a tab there.** Dragging a file or folder onto the tab row (or the `+`) opens a new tab in that directory, the way Windows Terminal does; dropping onto the pane itself still types the quoted path at the prompt.
- **Tab color from the keyboard.** The tab menu's six swatches are now also palette actions (`tab color: blue`, `tab color: none`, and friends), so recoloring a tab no longer needs the mouse and the actions bind in `keybindings.conf` like everything else.
- **Programming ligatures.** `==>`, `->`, `!=`, `::` and friends now shape through the font's ligature rules instead of rendering one glyph per cell — runs of punctuation with matching style are shaped as a string, composited into a strip, and cached, so the cost after the first sight of a sequence is a hash lookup. The bundled Maple Mono ships full ligatures; a font without them renders exactly as before. Letters never join a run (prose costs nothing), a run breaks at the cursor so the block cursor stays a clean cell, and `ligatures=false` in `config` turns the whole thing off.
- **Per-tab color.** Right-click a tab, pick `color`, and choose one of six swatches (or none). The tint washes the tab and its accent rail, follows theme switches because it's stored as a palette index rather than a fixed color, and comes back with session restore. The same marker Windows Terminal offers for telling six shells apart.
- **`font_weight` picks the base text weight.** `font_weight=light` (or `semibold`, `medium`, any name or 100–900 number) in `config` shapes regular text at that weight while bold text stays bold. For fonts whose regular cut runs thin at small sizes, or anyone who just prefers a heavier page.
- **`min_contrast` keeps text readable.** Set a WCAG contrast ratio (`min_contrast=3`, up to 21) and any foreground too close to its cell background is pushed toward white or black in linear light until it clears the bar, dim SGR text on a dark theme included. Windows Terminal's `minimumContrastRatio`, same scale.
- **Background image.** `background_image=<path.png>` draws a PNG behind every pane, scaled to cover the window, at `background_image_opacity=0.3` (0–1). Panes that set their own background keep it; everything on the default background lets the image show through.
- **Scripted key input.** `termie --drive script.txt` plays a timed key script — `500 key ctrl+shift+m`, `100 type hello`, one step per line in the `keybindings.conf` combo syntax — through the normal input path, with the clock starting when the first shell settles. The window opens non-activating, so an automated run can never steal focus from what you're doing or swallow keystrokes meant for another app. UI automation and demos without touching the desktop.
- **Script a window layout from the command line.** `termie new-tab -d C:\src ; split-pane -H --shell cmd` builds tabs and splits on launch using Windows Terminal's verb grammar: `nt`/`sp` for short, `-V` splits beside and `-H` below, `-d` and `--shell`/`-p` per pane. Scripted windows are ephemeral and never overwrite the saved session.
- **Search every open tab.** Run `tab search` from the command palette to open a numbered fuzzy switcher over the current window's tabs. Duplicate titles stay distinct by number, the current tab starts selected, keyboard and mouse selection both work, and the action can be assigned directly in `keybindings.conf`.
- **New window from the keyboard.** `Ctrl+Shift+N` opens one fresh window in the focused pane's shell and directory, matching Windows Terminal's standard chord without restoring an unrelated saved layout. The action is also on the command palette and is rebindable.
- **Select all.** `Ctrl+Shift+A` selects the focused pane's retained history and live screen for copying, with the action also available from the command palette and `keybindings.conf`. "Jump to attention" moves to `Ctrl+Alt+A` so the standard selection chord wins.
- **Word movement in mark mode.** `Ctrl+Left` / `Ctrl+Right` move to word boundaries, and adding `Shift` extends the selection by words. Movement crosses wrapped lines and scrollback instead of stopping at the visible row.
- **Termie can be your default terminal.** Run "default terminal" from the palette and every console app launched outside a terminal — the run box, the start menu, a double-clicked `.bat` — opens in a termie window instead of the legacy console host, the same OS handoff Windows Terminal uses (`ITerminalHandoff3` over the `HKCU\Console\%%Startup` delegation pair, Windows 11). The tab carries the app's title, resizing works, closing the tab ends the app, and the palette action toggles it back off — restoring whatever your default was before, which the uninstaller also does. Requires the inbox Windows Terminal package for its ConPTY host, present on Windows 11.
- **Taskbar jump list.** Right-click termie's taskbar icon (pinned or running) for "new window" tasks — plain, per built-in shell, and per custom profile — the same shortcut menu Windows Terminal ships. Backed by a new `--shell <name>` command-line flag, so `termie --shell cmd` (or a profile name) scripts a window into any shell too.
- **Mark mode: select and copy without the mouse.** `Ctrl+Shift+M` (Windows Terminal's chord, also "mark mode" on the palette) puts a selection cursor at the prompt: arrows move it, `Shift`+arrows extend a selection, `Home`/`End` jump within the line, `PgUp`/`PgDn` page through history, `Ctrl+Home`/`Ctrl+End` hit the ends of scrollback, and `Enter` (or `Ctrl+C`) copies and exits. `Esc` or a mouse click leaves the mode; the status bar shows `MARK MODE` while it's on. Rebindable in `keybindings.conf` like everything else.
- **Custom shell profiles.** `profile.<name>=<command line>` in `config` puts any shell on the palette as `new tab: <name>` — git-bash, nushell, an ssh session, whatever. Profiles restore with the session, duplicate like built-in shells, bind in `keybindings.conf` by label, and a profile name works as the `shell` default too. Quoted paths with spaces are handled.
- **Export scrollback.** "export scrollback" on the palette writes the focused pane's whole history plus screen to a timestamped `.txt` in Downloads — soft-wrapped lines join back into one logical line, exactly like copy. The status bar shows where it landed. Bindable in `keybindings.conf` like any palette action.
- **Mica backdrop.** `acrylic=true` (or `mica=true`) in `config` opts the window into the Windows 11 system backdrop, so the desktop glows through a translucent termie the way it does through the built-in apps. Cosmetic and safe everywhere: on Windows 10 the call is ignored and you keep flat opacity. Visible only with `opacity` below 100.
- **The scrollbar is now a stable dedicated rail.** It stays in the reserved right gutter whenever a pane has history, so a full-screen TUI cannot fight the thumb for the final text cell.
- **Prompt marks now show on the scrollbar.** The prompts Termie tracks through shell integration also appear as small pips on the rail, making the next command easier to spot before you jump to it.

### Terminal fidelity
- **Kitty unicode placeholders.** A virtual placement (`U=1`) plus cells of `U+10EEEE` with row/column diacritics now renders the image through the text layer itself — the mechanism image tools reach for when the bytes pass through something that rewrites the screen, because the cells scroll, copy, and reflow like any other text. The image id rides the cell's foreground color, omitted diacritics inherit from the cell to the left per the spec, and the picture aspect-fits its declared cell box. `kitten icat --unicode-placeholder` and tmux-aware image tools land on this path.
- **Chunked image uploads that name an id now display.** kitty clients send `i=` only in the first chunk of a streamed transfer; termie treated the id-less continuation chunks as a new anonymous upload, so the named image never finished and never drew. Continuations now attach to the transfer they belong to.
- **Huge images render instead of vanishing.** A kitty image wider or taller than the GPU atlas (2046 px) used to pack nowhere and silently never draw. It now box-filters down to fit while keeping its intended on-screen size, so a full-resolution photo piped through `icat` shows up like everywhere else.
- **Kitty graphics placements move the cursor.** Displaying an image now steps the cursor right by the placement's columns and down onto its last row, wrapping at the right edge and scrolling at the bottom exactly like printed text, with the protocol's `C=1` key opting out. Text after an image used to print straight through it; `icat`-style tools now lay out the way they do in kitty itself. The shipped ConPTY host passes the kitty escape codes through, so this works from a plain shell pipeline, not just termie-aware programs.
- **Kitty z-index.** The `z=` placement key stacks images: negative values draw beneath the pane's text (a watermark behind your output), zero and up draw above it, ordered by value within each side.
- **Scoped kitty deletes.** `a=d` now honors the `d=` target key: by image id, by id range (`d=r` with `x=`/`y=` bounds, which also clears virtual placements), by z layer, or whatever placement covers the cursor cell, with the uppercase forms also freeing the stored pixels. A bare delete still clears every placement, and a hard reset (`RIS`) now drops the decoded images along with the screen instead of pinning them until the pane closes.
- **Kitty keyboard protocol: "report all keys" and "associated text".** Progressive-enhancement flags 8 and 16 now stick (a `CSI ? u` query answers 27 with everything on). Every key reports as a `CSI u` escape code under flag 8 — plain printables, unmodified Enter/Tab/Backspace, and the modifier keys themselves with left/right variants — releases pair with flag 2, and flag 16 embeds the typed text's codepoints so editors can reconstruct input. Apps that requested these flags used to silently run on the disambiguate-only subset.
- **Torn-off windows report key releases and focus.** A pane popped out into its own window now forwards key releases when an app asked for event types (kitty flag 2) and sends `CSI I`/`CSI O` for mode 1004, matching the main window's behavior.
- **Sixel now works out of the box: installs ship a current ConPTY host.** The ConPTY inside Windows silently strips sixel (and other DCS/APC it doesn't know) before a terminal ever sees the bytes, so `img2sixel` and friends drew nothing on a stock install no matter how complete termie's decoder was. The installers now place Microsoft's `conpty.dll` + `OpenConsole.exe` (MIT, the `Microsoft.Windows.Console.ConPTY` package, pinned and hash-verified by `setup/fetch-conpty.ps1`) beside the exe, which termie's pty layer prefers over the inbox one.
- **Emoji ZWJ sequences occupy one cell and ligate.** 👩‍⚕️, ❤️‍🔥, 🏳️‍🌈 and friends used to break into one cell per member; now the whole sequence lives in a single wide cell (copy and selection return the full sequence) and is shaped inside the emoji font, so everything Segoe UI Emoji can ligate draws as one glyph. Multi-person families render as their first member — Windows 11's emoji font ships no composed family glyphs (Windows Terminal shows them split for the same reason).
- **Inverse text with default colors is visible again.** SGR 7 on a cell that never set explicit colors swapped the *default-fg* and *default-bg* sentinels, and the background resolver mapped both to the theme background — reverse-video prompts and status lines in default colors painted background-on-background. The bg slot now resolves a swapped default to the theme foreground.
- **Apps can start and stop cursor blink.** `CSI ? 12 h/l` (vim's `guicursor` blink path) now overrides the configured blink like DECSCUSR's blink bit already did, reports through DECRQM, and clears back to your default on a soft reset.
- **IRM insert mode.** `CSI 4 h` makes prints shift the rest of the line right instead of overwriting — old full-screen editors and vttest rely on it — with the ANSI DECRQM form (`CSI 4 $ p`) reporting the state and DECSTR putting it back to replace.
- **Apps can turn alternate scroll off.** Wheel-to-arrow-keys on the alt screen is still the default, but a full-screen app that wants the wheel dead (or handles it through mouse reporting) can now `CSI ? 1007 l` like in xterm and Windows Terminal, query the state back with DECRQM, and get the default restored on a full reset.
- **DECALN works — and `ESC 8` is safe again.** The vttest alignment pattern (`ESC # 8`) fills the screen with `E`, resets the margins and origin mode, and homes the cursor. It also used to fall through to the restore-cursor arm, so any program emitting it silently teleported the cursor instead.
- **Real tab stops.** HTS (`ESC H`) sets a stop, TBC (`CSI g`) clears one or all, and CHT / CBT (`CSI I` / `CSI Z`) hop forward and back over them — so the `tabs` command, `tput hts`, and anything laying out columns with custom stops lands where it aimed instead of on a hardcoded every-8 grid. Stops survive a resize (new columns pick up the default cadence).
- **CMD now has shell integration.** Its existing prompt is wrapped with OSC 133 marks and OSC 9;9 cwd updates, so prompt jumping, scrollbar ticks, tab labels, and "new tab here" work without changing the user's prompt text.
- **Prompt navigation survives a resize.** Shell-integration prompt marks now
  reflow with the text, so `Ctrl+Up` / `Ctrl+Down` keeps moving through command
  history after changing the window width instead of silently losing every mark.
- **Combined DECSET/DECRST applies every mode.** Sequences like `CSI ? 1000;1002;1003;1006 h` (how most full-screen apps enable mouse) used to take only the first parameter, so SGR mouse and any-event tracking never turned on together. Every `Ps` is applied now.
- **X10 mouse never emits high-bit bytes.** When an app enables mouse without SGR (`1006`) and the cell is past column/row 223, the report falls back to SGR instead of clamping to `0xFF` — those clamped bytes were poisoning UTF-8 input parsers and filling TUI composers with garbage.
- **Mouse motion is one report per cell.** Any-event tracking no longer re-emits the same CSI on every OS pointer sample while the cursor sits in one cell, which used to flood the child's input pipe under a moving mouse.
- **Leaving the alt screen (and DECSTR) resets more interaction state.** Focus reporting and synchronized-output frames join mouse, app-cursor, and kitty keyboard on the cleanup list, so a crashed or poorly-exiting full-screen app can't leave CSI `I`/`O` or a stuck DEC 2026 frame bleeding into the next prompt.
- **Honest `$TERM_PROGRAM`.** Children now see `TERM_PROGRAM=termie` and `TERM_PROGRAM_VERSION=<version>` instead of a spoofed host name. Capability is still negotiated the proper way (kitty keyboard CSI, XTVERSION, DA, XTGETTCAP). Apps that only enable features for a hard-coded allowlist can set `term_program=ghostty` (or another name) in `%APPDATA%\termie\config`.
- **ConPTY gets real pixel geometry** on open and resize when the renderer knows the cell size, so tools that ask the console for a window size in pixels stop seeing 0×0.

### Rendering
- **A grid cell is 24 bytes, down from 32.** The SGR attribute booleans and underline style pack into one 16-bit field, so every line of scrollback costs a quarter less: a 10,000-line history at 200 columns drops from 61 MB to 46 MB of cell storage.

## 0.3.2 — 2026-07-09

### Workflow
- **Duplicate tab** (`Ctrl+Shift+D`, and on the palette): opens a new tab running the focused pane's shell in its current directory — the same chord Windows Terminal uses. "New tab here" keeps opening the *default* shell there; duplicate carries the shell too, so a `cmd` or WSL tab duplicates as itself.
- **Tab reorder**: drag a tab along the strip to move it (it swaps past its neighbors live, like a browser), or nudge the active tab with `Ctrl+Shift+PgUp` / `Ctrl+Shift+PgDn` ("move tab left/right" on the palette). Tabs now also activate on press rather than release, matching every other tab strip.
- **Find follows the focused pane.** Switching tabs or panes — including after a split, pane close, tear-off, or focus move — while find is open re-runs the query against the newly focused grid, so highlights and next/prev no longer stick to the previous pane's match list. A shell dying in a *background* tab no longer poisons the open find against that tab either: the temporary owner switch is held, the viewer tab is restored, and find recomputes once against what you are still looking at. Closing the *active* tab also recomputes correctly (the pre-close identity is captured before the tab is removed, so find does not keep the killed pane's hits).
- **Find spans soft-wrapped lines.** A long URL or command that the terminal wrapped mid-line is still one logical string for search (same rule as copy), so `Ctrl+Shift+F` hits it; wide-glyph continuation cells no longer break a match.

### Terminal fidelity
- **XTGETTCAP capability queries** (`DCS + q`): answers the terminfo probes nvim and similar send when they can't trust `$TERM` (usually over ssh) — truecolor (`Tc`/`RGB`, `setrgbf`/`setrgbb`), styled underlines (`smulx`/`Su`), and color count — so those features light up without manual overrides. Unknown or malformed names get the standard failure reply instead of a hang.
- **DECRQSS status requests** (`DCS $ q`): programs can now read back the SGR pen (the probe tmux and the truecolor-detection scripts use — set a color, query it, look for the echo), the scroll region, the cursor style, DECSCL, and DECSCA, instead of getting silence; anything else gets the standard invalid reply rather than a hang.
- **Sixel graphics.** The other inline-image protocol — what `img2sixel`, `chafa --format sixels`, `lsix`, and gnuplot's `sixelgd` terminal emit — now decodes and draws through the same GPU image atlas as kitty graphics. The full drawing model is in: color registers with the VT340 default palette, RGB and DEC's blue-first HLS colorspaces, repeats, raster-attribute padding, and transparent holes. Images scroll inline with the text (cursor left on the line below, per DEC), DECSDM (mode 80) pins them to the top-left instead, and a hostile stream can't allocate past the same 64 MB budget the kitty path has. DA1 now answers `CSI ?62;4;22c` so tools discover sixel support the standard way, and XTSMGRAPHICS reports the color-register count and max geometry they size output from.

## 0.3.1 — 2026-07-02

### Installing & updating
- **A native installer replaces the WiX wizard.** `termie-<version>-setup.exe` is a single small window in termie's own instrument style — install path, four checkboxes, one INSTALL button. It installs per-user with **no admin prompt**, migrates an old MSI install (asking first), registers everything the MSI did (PATH, shortcuts, "Open in termie", Add/Remove Programs), and carries its own uninstaller. The MSI remains attached to releases for scripted installs.
- **termie updates itself now.** Once a day it quietly asks GitHub for the latest release; a newer one shows an `UPDATE x.y.z` chip on the status bar. Click it — or run **install update** from the palette, which also works as a manual "check now" — confirm, and the new build installs and relaunches with your session restored. Nothing downloads without that confirmation, pre-releases are never offered, and `update_check=false` turns the check off.

## 0.3.0 — 2026-07-02

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
- **Large inline images render instead of silently vanishing.** A kitty-graphics image bigger than the atlas's starting size (roughly 1000-2000 px on a side) failed to pack, was never cached, and re-rasterized every frame forever without ever appearing; the image atlas now grows to its maximum the same way the glyph atlas does, and a genuinely-too-crowded miss is cached instead of burning CPU each paint.
- **A packed tab strip can't slide under the window controls.** Tabs used to stop shrinking at 54px, so enough of them overlapped the split/settings/close buttons and pushed the new-tab "+" off-screen; tabs now shrink to fit (the close target retires below usable width — middle-click still closes), keeping every tab visible and clickable.
- **Underlines sit under the text now, not under the cell.** All decorations are anchored to the font's baseline: at line heights above 1.0 the underline used to float in the leading well below the descenders, the strikethrough sat off-center, and both looked detached. Underline now hugs the baseline, strikethrough crosses the x-height, and double underline stacks downward.
- **The undercurl is a smooth connected wave.** LSP/spellcheck squiggles (`SGR 4:3`) were drawn as disconnected one-pixel dots with gaps at every slope; each column now spans to the next column's phase, so the curl reads as one continuous line.
- **Colored underlines (SGR 58/59) and overline (SGR 53/55).** git-delta, and every editor that colors its squiggles independently of the text (red error curl under normal-colored code), now get exactly that — colon and semicolon forms both parsed, per-cell, in both the GPU renderer and the dev preview.
- **Whole-pixel column pitch.** The cell width was carried as a fractional glyph advance, landing every column on a different subpixel phase — visible as uneven glyph weight across a line and ±half-pixel box-drawing stem wobble, worst at 125%/150% display scaling. Cell width and the baseline are now whole physical pixels, so every column renders identically.
- **Four new built-in themes: Catppuccin Mocha, Gruvbox, Tokyo Night, and Nord** — the schemes people actually ask a terminal for, each with a matched chrome ladder (tab bar, status bar, rules) so the whole window wears the theme, not just the cells. The settings panel's theme row grew into a two-row gallery with live swatches, and `colors.conf` still overrides any of them per-key.
- **Dim text (SGR 2) is legible now** — it was multiplied to roughly a fifth of its linear brightness and read as near-invisible; it now sits at the conventional "clearly dimmer, still readable" level.
- **The unfocused pane's cursor is a hollow block** instead of a translucent fill, so the character under it stays readable across a cockpit of panes — the same convention Windows Terminal and Ghostty use.

### Workflow
- **Closing the window asks first when work is running.** The window's X, `Alt+F4`, and the quit action used to kill every pane instantly; with more than one pane or tab alive they now get the same confirm that closing a multi-pane tab always had — so a stray close can't take down a window full of working agents. Torn-off windows get the same guard on their own close button. A single idle shell still closes without ceremony.
- **Config mistakes are reported instead of swallowed.** termie now writes a log to `%APPDATA%\termie\termie.log` (bounded, restarted past 512 KB): a mistyped `colors.conf` line, a bad keybinding combo, or an unknown `config` key lands there with a message instead of vanishing — the release build has no console, so until now there was nowhere for any of it to go.
- **Keyboard scrollback navigation**: `Shift+PgUp`/`Shift+PgDn` scroll a page of history and `Ctrl+Shift+Home`/`End` jump to either end (the conhost/Windows Terminal chords), all rebindable. A **clear scrollback** action joins the palette, and "scroll to top / bottom" are there too.
- **Prompt jump works out of the box.** `Ctrl+Up`/`Ctrl+Down` jump between prompts — the feature was fully built but nothing ever emitted the OSC 133 marks it listens for; the injected pwsh prompt hook now emits them. The hook also **wraps your own prompt instead of replacing it**: with profile loading on, starship / oh-my-posh keep working and still get cwd tracking and prompt marks.
- **Alt+drag selects a rectangle** (block/column selection, like Windows Terminal and WezTerm): grab a column of values out of `ls` output or a log without dragging whole lines along. Copy joins the rows with newlines and trims each row's trailing spaces. Inside a mouse-capturing TUI, add shift the way you would for any selection (`Shift+Alt+drag`).
- **Shift+click extends the selection** to the clicked cell instead of starting over — the anchor-extend every other terminal and editor does. Works with shift-drag, respects copy-on-select, and still lets shift bypass an app's mouse capture first.
- **A bell in a background tab now shows a dot on that tab.** Run an agent or a long build in another tab and the tab marks itself the moment its shell rings the bell (`BEL` — Claude Code and most CI-ish tools ring it when they finish or need input), so you can see *which* tab wants you instead of cycling through them. Viewing the tab clears the dot; while a tab is hovered the dot yields to the close button.
- **A bell while the window is unfocused flashes the taskbar button** (until you refocus, the standard Windows attention signal) — so an agent finishing in a minimized or covered termie still reaches you. Torn-off pane windows get the same treatment, scoped to their own taskbar button and tabs.
- **Program notifications ring through the same channel**: an iTerm2-style `OSC 9 ; message` or rxvt/tmux `OSC 777 ; notify` now counts as a bell — tab dot, taskbar flash, border flash — instead of being dropped. ConEmu's numeric `OSC 9` subcommands (progress, cwd) are still told apart and handled as before.
- **The notification's text shows on the status bar** for a few seconds — a bell-marked readout in the right cluster ("claude: waiting for your approval"), so you know *why* a tab is dotted before you switch to it. Torn-off windows show it on their own bar.

### Resiliency & security
(first published here; previously tagged as 0.2.11-rc1)
- **COM API resource cleanup**: an RAII guard balances COM initializations on the GUI thread, preventing reference-count leaks when querying Explorer directories.
- **Robust plugin installation**: a failed cross-volume plugin install now cleans up its partial folder instead of leaving a corrupted installation.
- **Plugin IPC message length limits**: a 256 KB per-line cap on plugin stdout; an oversized line is logged, discarded, and the stream resynchronized, so a rogue plugin can't inflate memory.
- **Color override warnings**: `colors.conf` parsing reports missing `=` signs and unparseable colors.

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
