# relay — termie reference plugin (the bus)

Demonstrates the Phase-3 in-process plugin **bus**: `subscribe` to a topic,
`publish` to it, and receive other plugins' messages as `message` events.

## What it does

- Subscribes to the `chat` topic and shows received messages in a `relay`
  widget in the dock (last 5).
- When a pane rings the bell, it `publish`es `"bell rang"` on `chat` — any other
  plugin subscribed to `chat` (in the same termie) receives it as a `message`.

So with `relay` plus another bus-aware plugin running, a bell in one pane fans
out over the bus. This is the in-process half of "sessions talking to each
other". The cross-*machine* half is a plugin's own job: open a socket here and
bridge it onto the bus (the host bus is local by design in v1).

## Build & install

Independent crate (own `[workspace]`), so it never affects termie's build.

```powershell
cd plugins/relay
cargo build --release
$dst = "$env:APPDATA\termie\plugins\relay"
New-Item -ItemType Directory -Force $dst | Out-Null
Copy-Item target/release/relay.exe $dst
Copy-Item plugin.json $dst
```

## Protocol

See `../README.md`. Bus verbs: `subscribe {topic}`, `publish {topic, body}`;
the host delivers `message {from, topic, body}` to subscribers (topic `"*"`
matches all; a publisher never receives its own message).
