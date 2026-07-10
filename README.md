# termie

A fast, lightweight GPU terminal multiplexer for Windows — tabs, split panes, and many shells across many repos in one window.

![termie](shot.png)

- downloads: [GitHub Releases](https://github.com/lintowe/termie/releases) (Windows x64 installer)
- plugins: registry at [`lintowe/termie-plugins`](https://github.com/lintowe/termie-plugins)
- license: MIT OR Apache-2.0

> Early but daily-usable. Windows-first; the terminal core is portable.

## features

GPU-rendered (wgpu glyph-atlas), a ~8.8 MB binary, and a lean dependency tree. A pre-warmed shell pool keeps a started PowerShell ready, so new tabs and splits feel instant and the window appears before any shell finishes spawning.

Tabs and recursive split panes — split vertically or horizontally, drag tabs to reorder them, drag dividers, swap panes, tear a pane off into its own window, and broadcast input to every pane in a tab. Splits and "new tab here" open in the focused pane's directory; pick a per-tab shell (`pwsh` / `cmd` / `wsl`) from the command palette. A bell in a background tab dots that tab, and a bell while the window is unfocused flashes the taskbar — so a finished agent or build finds you, not the other way around. The taskbar icon carries a jump list: right-click it for a new window in any shell or custom profile (`termie --shell cmd` from a script does the same).

Real terminal emulation: a [vte](https://github.com/alacritty/vte)-based parser, alt screen, scroll regions, mouse reporting, bracketed paste, OSC 7 cwd (tab labels + window title), reflow on resize, the kitty keyboard protocol (so `Shift+Enter` inserts a newline in TUIs), OSC 8 hyperlinks, OSC 52 clipboard writes, OSC 4/10/11/12 color queries, OSC 9;4 taskbar progress, underline styles, strikethrough, blink, and DEC 2026 synchronized output for tear-free frames.

Inline images via the kitty graphics protocol (raw RGB / RGBA / PNG) and sixel — `img2sixel`, `chafa`, `lsix`, and anything else that probes DA1 finds both — plus full-color emoji, all packed into a dedicated RGBA atlas beside the glyph cache. Installs ship a current ConPTY host (`conpty.dll` + `OpenConsole.exe`, MIT, from microsoft/terminal) beside the exe, because the ConPTY built into Windows strips sixel before a terminal ever sees it. IME composition, a screen-reader path via AccessKit, and session restore (tab + split layout) with crash recovery.

On Windows 11 termie can be the **default terminal**: run "default terminal" from the palette once, and console apps launched from the run box, the start menu, or a double-clicked script open in a termie window instead of the legacy console host. The same palette action turns it back off (your previous choice is restored), and so does uninstalling.

A command palette (`Ctrl+P`) for fuzzy access to every action, plus a searchable numbered tab switcher (`tab search` in the palette) for crowded windows. Seven built-in themes — three house schemes plus Catppuccin Mocha, Gruvbox, Tokyo Night, and Nord — a bundled Maple Mono Nerd Font, adjustable font size / padding / cursor / opacity, and per-user `colors.conf` and `keybindings.conf`. An optional Quake-style drop-down (`quake_key`).

A plugin system: plugins run as separate processes over a small JSON protocol, render widgets in a side dock (text or drawn graphics), talk over an in-process bus, and can be confined to a Windows AppContainer. An in-app marketplace browses and installs them.

## install

Download `termie-<version>-setup.exe` from the [latest release](https://github.com/lintowe/termie/releases/latest) and run it — a small native installer in termie's own style, not a wizard. It installs per-user (no admin prompt), and the options are right on its one page: `PATH`, Start-menu and desktop shortcuts, and the "Open in termie" right-click entry. It shows up in Add/Remove Programs, replaces any older install (including the previous MSI, after asking), and uninstalls cleanly. The build is unsigned, so SmartScreen may warn first: **More info → Run anyway**.

termie checks for a newer release once a day and shows a small `UPDATE` chip on the status bar when one exists — click it (or run "install update" from the palette) and the new version installs and relaunches with your session restored. Nothing ever downloads without that confirmation; turn the check off entirely with `update_check=false` in `config`. An MSI is still attached to each release for anyone scripting installs.

## keybindings

| key | action |
|---|---|
| `Ctrl+T` / `Ctrl+W` | new / close tab |
| `Ctrl+Shift+D` | duplicate tab (same shell and directory) |
| `Ctrl+Shift+N` | new window (same shell and directory) |
| `Ctrl+Tab` / `Ctrl+1`..`9` | next / nth tab |
| `Ctrl+Shift+PgUp` / `Ctrl+Shift+PgDn` | move tab left / right (tabs also drag to reorder) |
| `Ctrl+Shift+E` / `Ctrl+Shift+O` | split vertical / horizontal |
| `Ctrl+P` | command palette |
| `Ctrl+Shift+P` | pane mode (move / resize / zoom / close) |
| `Ctrl+Shift+M` | mark mode (arrows move, Ctrl+arrows jump words, Shift extends, Enter copies) |
| `Ctrl+Shift+A` | select all text in the focused pane |
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | copy / paste (also `Ctrl+Insert` / `Shift+Insert`) |
| `Ctrl+Shift+F` | find in scrollback |
| `Ctrl+Shift+B` | broadcast input to every pane |
| `Ctrl+Alt+A` | jump to the next pane that failed, rang, or finished |
| `Ctrl+Shift+W` | close pane |
| `Ctrl+Up` / `Ctrl+Down` | jump to previous / next prompt (marked on the scrollbar) |
| `Shift+PgUp` / `Shift+PgDn` | scroll a page of history |
| `Ctrl+Shift+Home` / `Ctrl+Shift+End` | scroll to the top / bottom |
| `F11` | fullscreen |
| `Ctrl`+wheel | font zoom |

Every binding is rebindable (or unbindable) in `keybindings.conf`; the full list is in the command palette.

## shells

Auto-detects and prefers `pwsh` → `powershell` → `cmd`, with WSL also selectable. PowerShell launches `-NoLogo -NoProfile` (profile loading is opt-in) with telemetry and update checks off for a fast prompt. PowerShell and CMD both emit shell-integration prompt marks and current-directory updates, so prompt navigation, scrollbar marks, tab labels, and "new tab here" work in either shell. Set the default in settings, or open a one-off tab in any shell from the palette.

Any other shell works as a custom profile — add `profile.<name>=<command line>` to `config` (quote paths with spaces):

    profile.git-bash="C:\Program Files\Git\bin\bash.exe" -i -l
    profile.nu=nu.exe

Each profile shows up on the palette as `new tab: <name>`, can be bound in `keybindings.conf` by that same label, restores with the session, and duplicates like the built-in shells. A profile name also works as the `shell` default.

A shell or profile can carry its own theme — `theme.<name>=<theme>` paints that shell's panes with a different palette while the window chrome keeps the global theme, so a WSL split reads apart from your PowerShell pane at a glance:

    theme.wsl=gruvbox
    theme.git-bash=nord

## configuration

Drop files in `%APPDATA%\termie\`:

- `config` — general settings the in-app panel also writes (`shell`, `theme`, `scrollback`, …). Opt-ins live here too: `quake_key=ctrl+grave` (drop-down hotkey), `plugin_sandbox=appcontainer` (sandbox every plugin), `latency_hud=true` (input-to-photon readout), `acrylic=true` (Win11 Mica backdrop behind a translucent window; `mica=true` also works), `right_click=paste` (right-click pastes, or copies an active selection and clears it, instead of opening the context menu — Windows Terminal style; `Shift+right-click` still opens the menu), `term_program=ghostty` (override `$TERM_PROGRAM` for apps that only enable the kitty keyboard protocol on a hard-coded host allowlist; default is `termie`).
- `colors.conf` — override theme colors, one `key=color` per line (`fg`, `bg`, `cursor`, `sel`, `ansi0`..`ansi255`; `#rrggbb`, `#rgb`, or `r,g,b`).
- `keybindings.conf` — rebind keys, one `combo=action` per line, e.g. `ctrl+alt+t=new tab here`.

Mistyped lines in any of these are reported to `%APPDATA%\termie\termie.log`.

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
cargo build --release  # optimized, ~8.8 MB
```

## plugins

Plugins are separate processes termie talks to over newline-delimited JSON, so a plugin can be written in any language and be as heavy as it likes while the core stays lean. They render widgets in a side dock — Tier-1 text or Tier-2 immediate-mode graphics — talk to each other over an in-process bus, and can be confined to a Windows AppContainer (opt-in). The in-app marketplace (palette → "plugins") browses, installs, enables/disables, and removes them; the registry — plugin source plus the catalog — lives at [`lintowe/termie-plugins`](https://github.com/lintowe/termie-plugins), which is also where you contribute one.

## license

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Bundled fonts and other third-party material keep their own licenses — see [THIRDPARTY.md](THIRDPARTY.md).

## built with

[wgpu](https://github.com/gfx-rs/wgpu) · [winit](https://github.com/rust-windowing/winit) · [vte](https://github.com/alacritty/vte) · [portable-pty](https://github.com/wezterm/wezterm/tree/main/pty) · [cosmic-text](https://github.com/pop-os/cosmic-text)
