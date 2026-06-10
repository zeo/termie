# termie

A fast, lightweight GPU terminal multiplexer for Windows — tabs, split panes, and many shells across many repos in one window.

![termie](shot.png)

- downloads: [GitHub Releases](https://github.com/lintowe/termie/releases) (Windows x64 zip)
- plugins: registry at [`lintowe/termie-plugins`](https://github.com/lintowe/termie-plugins)
- license: MIT OR Apache-2.0

> Early but daily-usable. Windows-first; the terminal core is portable.

## features

GPU-rendered (wgpu glyph-atlas), a ~7.6 MB binary, and a lean dependency tree. A pre-warmed shell pool keeps a started PowerShell ready, so new tabs and splits feel instant and the window appears before any shell finishes spawning.

Tabs and recursive split panes — split vertically or horizontally, drag dividers, swap panes, tear a pane off into its own window, and broadcast input to every pane in a tab. Splits and "new tab here" open in the focused pane's directory; pick a per-tab shell (`pwsh` / `cmd` / `wsl`) from the command palette.

Real terminal emulation: a [vte](https://github.com/alacritty/vte)-based parser, alt screen, scroll regions, mouse reporting, bracketed paste, OSC 7 cwd (tab labels + window title), reflow on resize, the kitty keyboard protocol (so `Shift+Enter` inserts a newline in TUIs), OSC 8 hyperlinks, OSC 52 clipboard writes, OSC 4/10/11/12 color queries, OSC 9;4 taskbar progress, underline styles, strikethrough, blink, and DEC 2026 synchronized output for tear-free frames.

Inline images via the kitty graphics protocol (raw RGB / RGBA / PNG) and full-color emoji, both packed into a dedicated RGBA atlas beside the glyph cache. IME composition, a screen-reader path via AccessKit, and session restore (tab + split layout) with crash recovery.

A command palette (`Ctrl+Shift+P`) for fuzzy access to every action. Three built-in themes, a bundled Maple Mono Nerd Font, adjustable font size / padding / cursor / opacity, and per-user `colors.conf` and `keybindings.conf`. An optional Quake-style drop-down (`quake_key`).

A plugin system: plugins run as separate processes over a small JSON protocol, render widgets in a side dock (text or drawn graphics), talk over an in-process bus, and can be confined to a Windows AppContainer. An in-app marketplace browses and installs them.

## install

Download `termie-<version>-windows-x64.zip` from the [latest release](https://github.com/lintowe/termie/releases/latest), extract it anywhere, and run `termie.exe`. Keep the `assets` folder beside the executable — that's where the bundled font lives. The build is unsigned, so SmartScreen may warn on first launch: **More info → Run anyway**.

## keybindings

| key | action |
|---|---|
| `Ctrl+T` / `Ctrl+W` | new / close tab |
| `Ctrl+Tab` / `Ctrl+1`..`9` | next / nth tab |
| `Ctrl+Shift+E` / `Ctrl+Shift+O` | split vertical / horizontal |
| `Ctrl+Shift+P` | command palette |
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | copy / paste (also `Ctrl+Insert` / `Shift+Insert`) |
| `Ctrl+Shift+F` | find in scrollback |
| `Ctrl+Shift+B` | broadcast input to every pane |
| `Ctrl+Shift+W` | close pane |
| `Ctrl+Up` / `Ctrl+Down` | jump to previous / next prompt |
| `F11` | fullscreen |
| `Ctrl`+wheel | font zoom |

Every binding is rebindable (or unbindable) in `keybindings.conf`; the full list is in the command palette.

## shells

Auto-detects and prefers `pwsh` → `powershell` → `cmd`, with WSL also selectable. PowerShell launches `-NoLogo -NoProfile` (profile loading is opt-in) with telemetry and update checks off for a fast prompt. Set the default in settings, or open a one-off tab in any shell from the palette.

## configuration

Drop files in `%APPDATA%\termie\`:

- `config` — general settings the in-app panel also writes (`shell`, `theme`, `scrollback`, …). Opt-ins live here too: `quake_key=ctrl+grave` (drop-down hotkey), `plugin_sandbox=appcontainer` (sandbox every plugin), `latency_hud=true` (input-to-photon readout).
- `colors.conf` — override theme colors, one `key=color` per line (`fg`, `bg`, `cursor`, `sel`, `ansi0`..`ansi255`; `#rrggbb`, `#rgb`, or `r,g,b`).
- `keybindings.conf` — rebind keys, one `combo=action` per line, e.g. `ctrl+alt+t=new tab here`.

## build from source

Requires the [Rust toolchain](https://rustup.rs/) (stable).

```powershell
git clone https://github.com/lintowe/termie
cd termie
powershell -ExecutionPolicy Bypass -File install.ps1
```

This builds in release, installs to `%LOCALAPPDATA%\Programs\termie`, bundles the fonts, adds the directory to your user `PATH`, and registers an "Open in termie" context-menu entry. Restart your shell, then run `termie`. Remove it with `uninstall.ps1`. To run without installing, use `cargo run --release`.

```powershell
cargo build            # debug
cargo test             # unit tests (incl. golden snapshots)
cargo clippy --all-targets
cargo build --release  # optimized, ~7.6 MB
```

## plugins

Plugins are separate processes termie talks to over newline-delimited JSON, so a plugin can be written in any language and be as heavy as it likes while the core stays lean. They render widgets in a side dock — Tier-1 text or Tier-2 immediate-mode graphics — talk to each other over an in-process bus, and can be confined to a Windows AppContainer (opt-in). The in-app marketplace (palette → "plugins") browses, installs, enables/disables, and removes them; the registry lives at [`lintowe/termie-plugins`](https://github.com/lintowe/termie-plugins). Two reference plugins ship in [`plugins/`](plugins/).

## license

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Bundled fonts and other third-party material keep their own licenses — see [THIRDPARTY.md](THIRDPARTY.md).

## built with

[wgpu](https://github.com/gfx-rs/wgpu) · [winit](https://github.com/rust-windowing/winit) · [vte](https://github.com/alacritty/vte) · [portable-pty](https://github.com/wezterm/wezterm/tree/main/pty) · [cosmic-text](https://github.com/pop-os/cosmic-text)
