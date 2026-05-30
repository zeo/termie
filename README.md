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
- **Command palette.** `Ctrl+Shift+P` for fuzzy access to every action.
- **Real terminal emulation.** [vte](https://github.com/alacritty/vte)-based parser, alt screen, scroll regions, mouse reporting (SGR + legacy), bracketed paste, OSC-7 cwd (tab labels + window title), URL detection (Ctrl-click to open), DECSCUSR cursor shapes, and DEC 2026 synchronized output for tear-free frames.
- **Themes + fonts.** Three built-in themes (Instrument / Koi / Paper), bundled Maple Mono Nerd Font, lazy system-font discovery, adjustable font size, padding, cursor shape/blink, and window opacity — all in an in-app settings panel.
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

Open the command palette (`Ctrl+Shift+P`) for settings, theme cycling, pane mode, and the plugins marketplace.

## Shells

Auto-detects and prefers `pwsh` → `powershell` → `cmd`; the shell is selectable in settings. PowerShell launches with `-NoLogo -NoProfile` (profile loading is opt-in) and telemetry/update-check disabled for a fast prompt.

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

## Built with

[wgpu](https://github.com/gfx-rs/wgpu) · [winit](https://github.com/rust-windowing/winit) · [vte](https://github.com/alacritty/vte) · [portable-pty](https://github.com/wezterm/wezterm/tree/main/pty) · [cosmic-text](https://github.com/pop-os/cosmic-text)
