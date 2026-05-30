# tamagotchi — termie reference plugin

A tiny pet that lives in termie's plugin dock. It's the first-party reference
for the v1 plugin protocol: zero dependencies, ~150 lines, demonstrates
declaring/updating a Tier-1 widget and reacting to host events.

## What it does

- Declares a `pet` widget, then repaints it every ~2s as the pet gets hungrier
  and a little less joyful over time.
- Reacts to host events: a terminal `bell` startles it happy; switching pane
  focus cheers it up slightly.
- Exits cleanly when termie closes its stdin or sends `shutdown`.

## Build & install

This is an independent crate (its own `[workspace]`), so it does not affect
termie's build.

```powershell
cd plugins/tamagotchi
cargo build --release
# install into termie's plugin dir
$dst = "$env:APPDATA\termie\plugins\tamagotchi"
New-Item -ItemType Directory -Force $dst | Out-Null
Copy-Item target/release/tamagotchi.exe $dst
Copy-Item plugin.json $dst
```

Relaunch termie — the pet appears in the right-side dock. (termie spawns enabled
plugins after the window is shown, so it never slows startup.)

## How it talks to termie

Newline-delimited JSON: host events arrive on stdin, commands go out on stdout,
stderr is for logs. See `../README.md` for the full protocol.
