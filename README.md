<div align="center">

# termie

**A fast, lightweight GPU terminal multiplexer for Windows — a cockpit for running AI TUIs across many repos at once.**

</div>

termie is a from-scratch terminal emulator + multiplexer written in Rust. It renders on the GPU (wgpu) in a single-process [winit](https://github.com/rust-windowing/winit) app, ships as a ~7.6 MB binary, and is built around one job: driving several long-running CLI sessions (many shells) across different repositories without a pile of disconnected windows.

> Status: early but daily-usable. Windows-first; the terminal core is portable.

## Highlights

- **GPU-rendered, lightweight.** wgpu glyph-atlas rendering, an instrument-panel aesthetic, a ~7.6 MB stripped release binary, and a lean dependency tree.
- **Fast to open shells.** A pre-warmed shell pool keeps a started PowerShell ready so new tabs/splits feel instant; the window appears before any shell finishes spawning.
- **Tabs + recursive split panes.** Split vertically/horizontally, drag dividers, swap panes, and broadcast input to every pane in a tab (cockpit mode).
- **Built for many repos.** Splits and a "new tab here" command open in the focused pane's directory; per-tab shell choice (`pwsh` / `cmd` / `wsl`) from the palette; drag a file in to type its path; right-click to copy the selection or paste.
- **Command palette.** `Ctrl+Shift+P` for fuzzy access to every action.
- **Faithful keyboard + modern escapes.** The kitty keyboard protocol (so `Shift+Enter` inserts a newline in TUIs, plus accurate modified-key reporting), OSC 8 clickable hyperlinks, OSC 52 clipboard writes, OSC 4/10/11/12 color queries, and curly/double/dotted underline styles, strikethrough, and blink.
- **Real terminal emulation.** [vte](https://github.com/alacritty/vte)-based parser, alt screen, scroll regions, mouse reporting (SGR + legacy), bracketed paste, OSC-7 cwd (tab labels + window title), URL detection (Ctrl-click to open), DECSCUSR cursor shapes, and DEC 2026 synchronized output for tear-free frames.
- **Themes, fonts, and customization.** Three built-in themes (Instrument / Koi / Paper), bundled Maple Mono Nerd Font, lazy system-font discovery, adjustable font size, padding, cursor shape/blink, and window opacity in an in-app settings panel — plus per-user color overrides (`colors.conf`) and rebindable keys (`keybindings.conf`).
- **Plugin system + in-app marketplace.** Extend termie without bloating the core: plugins run as separate OS processes over a small JSON protocol, so they can be written in any language and can't slow startup or crash the app. See [docs/plugin-system-plan.md](docs/plugin-system-plan.md) and [plugins/README.md](plugins/README.md).

## Install

Requires the [Rust toolchain](https://rustup.rs/) (stable). From a clone:

```powershell
git clone https://github.com/lintowe/termie
cd termie
powershell -ExecutionPolicy Bypass -File install.ps1
```

This builds in release, installs to `%LOCALAPPDATA%\Programs\termie`, bundles the fonts, and adds the directory to your user `PATH`. Restart your shell, then run `termie`. Remove it with `uninstall.ps1`.

To just run it without installing:

```powershell
cargo run --release
```

## Keybindings

| Key | Action |
|-----|--------|
| `Ctrl+T` | new tab |
| `Ctrl+W` | close tab |
| `Ctrl+Tab` | next tab |
| `Ctrl+1`..`9` | go to tab |
| `Ctrl+Shift+E` | split vertical |
| `Ctrl+Shift+O` | split horizontal |
| `Ctrl+Shift+P` | command palette |
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | copy / paste |
| `Ctrl+Shift+W` | close pane |
| `Ctrl+Shift+B` | broadcast input to all panes |
| `Ctrl+Shift+F` | find in scrollback |
| `Ctrl+Up` / `Ctrl+Down` | jump to previous / next shell prompt |

In find, `Enter` / `Shift+Enter` step through matches and `Esc` closes. Double-click selects a word, triple-click selects the line. Prompt jumping uses OSC 133 marks, emitted by most shells once their shell-integration hook is enabled (starship, or the zsh/bash/fish integration scripts, or PowerShell's `PSReadLine`); without it the prompt keys pass through to the running program.

Open the command palette (`Ctrl+Shift+P`) for settings, theme cycling, pane mode, and the plugins marketplace.

## Shells

Auto-detects and prefers `pwsh` → `powershell` → `cmd`, with **WSL** also selectable; the default shell is set in settings, and the palette's "new tab: pwsh / cmd / wsl" opens a one-off tab in any shell. PowerShell launches with `-NoLogo -NoProfile` (profile loading is opt-in) and telemetry/update-check disabled for a fast prompt.

## Customization

Drop files in `%APPDATA%\termie\`:

- **`colors.conf`** — override theme colors, one `key=color` per line. Keys include `fg`, `bg`, `cursor`, `sel`, and `ansi0`..`ansi255`; colors are `#rrggbb`, `#rgb`, or `r,g,b`. Overrides apply on top of the selected built-in theme.
- **`keybindings.conf`** — rebind keys, one `combo=action` per line, e.g. `ctrl+alt+t=new tab here`. Action names match the command-palette entries; bindings take precedence over the built-in defaults.

## Plugins

Plugins are separate processes termie talks to over newline-delimited JSON, so a plugin can be any language and as heavy as it likes while the core stays lean. They render **Tier-1 widgets** in a side dock and can talk to each other over an in-process bus. The in-app marketplace (palette → "plugins") browses, installs, enables/disables, and removes them; sensitive permissions are off by default and shown at install. Two reference plugins live in [`plugins/`](plugins/): a tamagotchi pet and a session relay.

## Building & development

```powershell
cargo build            # debug
cargo test             # unit tests
cargo clippy --all-targets
cargo build --release  # optimized, ~7.6 MB
```

CI (GitHub Actions) runs build + tests, clippy, a `cargo-audit` security scan, and builds the bundled example plugins on every push.

### Headless rendering harness

Terminal and rendering changes are verifiable without opening a window — they run through the real parser, grid, and glyph atlas:

```powershell
cargo run -- --termview --scenario sgr      # dump the grid + state as text
cargo run -- --termview --seq "\e[31mhi"    # feed an escape sequence (also --file, --resize COLSxROWS)
cargo run -- --termview --scenario wrap --png out.png   # render the same scene to an image
```

`cargo test golden` checks a set of fixed scenarios (SGR, diff bars, soft-wrap, reflow grow/shrink, background-color erase, kitty queries, OSC, cursor moves, underline styles) against checked-in snapshots in [`tests/golden/`](tests/golden). A terminal or rendering change shows up as a diff in the failing test. After an **intended** change, re-bless the snapshots and review the diff before committing:

```powershell
$env:BLESS=1; cargo test golden; $env:BLESS=$null
git diff tests/golden    # read exactly what changed
```

### Layout

```
src/
  main.rs         App, event loop, tab/pane tree, shell pool
  render/         wgpu renderer, glyph atlas, shaders, chrome/UI
  term.rs         vte Perform: CSI/OSC/SGR handling
  grid.rs         terminal grid, scrollback, wrapping
  pty.rs          ConPTY via portable-pty
  plugin/         plugin host, JSON protocol, manifest, marketplace
  color.rs        themes + sRGB conversion
plugins/          first-party reference plugins (independent crates)
```

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Bundled fonts and other third-party material are covered by their own licenses — see [THIRDPARTY.md](THIRDPARTY.md).

## Built with

[wgpu](https://github.com/gfx-rs/wgpu) · [winit](https://github.com/rust-windowing/winit) · [vte](https://github.com/alacritty/vte) · [portable-pty](https://github.com/wezterm/wezterm/tree/main/pty) · [cosmic-text](https://github.com/pop-os/cosmic-text)
