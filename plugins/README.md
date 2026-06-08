# termie plugins

Plugins extend termie **without bloating the core**. A plugin is a separate OS
process that termie launches and talks to over a tiny line protocol. Because
plugins run out of process, a plugin can be written in any language and be as
heavy as it likes — the core stays lean and fast, and a crashing plugin can't
take termie down.

See `docs/plugin-system-plan.md` for the full design and rationale.

> Status: implemented through Tier-2 rendering, with an opt-in OS sandbox. The
> plugin host and protocol, Tier-1 widgets and the dock, Tier-2 immediate-mode
> drawing, the in-process bus, local install/enable/disable, and the in-app store
> all work, and `plugin_sandbox=appcontainer` confines each plugin to a Windows
> AppContainer. A cross-machine bus is possible later work. The protocol below is
> the **v2** contract (`api_version` 2); a v1 plugin that never sends a draw list
> keeps working unchanged.

## Sandboxing (opt-in)

By default a plugin runs as a normal subprocess with the user's rights — crash
isolation, not privilege isolation. Setting `plugin_sandbox=appcontainer` in
`%APPDATA%\termie\config` instead launches every plugin inside a Windows
AppContainer: low integrity, with no access to the user's files, registry,
network, windows, or other processes unless granted. A plugin's `network`
permission maps to the internetClient capability; the plugin's own install
directory is granted read+execute so its executable loads. A plugin that needs
un-granted access won't work sandboxed, so this is off by default.

## Where plugins live

Installed plugins live under `%APPDATA%\termie\plugins\<id>\`, one directory per
plugin, each with a `plugin.json` manifest. First-party plugins are developed in
this `plugins/` folder in the repo.

## Manifest: `plugin.json`

```json
{
  "id": "tamagotchi",
  "name": "Tamagotchi",
  "version": "0.1.0",
  "api_version": 2,
  "entry": { "cmd": "tamagotchi.exe", "args": [] },
  "permissions": []
}
```

- `id` — unique; defaults to the directory name if omitted.
- `api_version` — the protocol version the plugin is built against. A Tier-1
  plugin may still declare `1`; declare `2` if you send a Tier-2 draw list.
- `entry.cmd` — the executable to run. Relative paths resolve against the plugin
  directory; absolute paths are used as-is.
- `entry.args` — optional argument list.
- `permissions` — requested capabilities, shown to the user at install. Sensitive
  ones (`read_output`, `write_pty`) are off unless granted. Declaring/updating
  widgets, the bus, and notifications need no permission.

## Protocol

Newline-delimited JSON. One JSON object per line.
- termie → plugin on the plugin's **stdin** (host events).
- plugin → termie on the plugin's **stdout** (commands).
- the plugin's **stderr** is for its own logs.

Every message is an object with a `"t"` type tag. Unknown types are ignored
rather than fatal, so a newer plugin talking to an older termie (or vice versa)
degrades gracefully instead of breaking the stream.

### Host → plugin events

| `t`              | fields                          |                                  |
|------------------|---------------------------------|----------------------------------|
| `hello`          | `api_version`, `permissions[]`  | sent once on startup (handshake) |
| `focus_changed`  | `pane`                          |                                  |
| `tab_changed`    | `tab`                           |                                  |
| `cwd_changed`    | `cwd`                           |                                  |
| `bell`           | `pane`                          |                                  |
| `widget_clicked` | `id`                            |                                  |
| `message`        | `from`, `topic`, `body`         | from another plugin via the bus  |
| `shutdown`       | —                               |                                  |

`hello.api_version` is the host's protocol version — read it to decide whether to
send a Tier-2 draw list.

### Plugin → termie commands

| `t`              | fields                              |                                     |
|------------------|-------------------------------------|-------------------------------------|
| `ready`          | `name`, `api_version`               | announce yourself after `hello`     |
| `declare_widget` | `widget`                            |                                     |
| `update_widget`  | `widget`                            |                                     |
| `notify`         | `text`                              |                                     |
| `write_pty`      | `data`                              | requires the `write_pty` permission |
| `publish`        | `topic`, `body`                     | publish to the bus                  |
| `subscribe`      | `topic` (`"*"` = all)               |                                     |

### The `widget` object

| field      | type       |                                                          |
|------------|------------|----------------------------------------------------------|
| `id`       | string     | stable per widget; updates upsert by `id`                |
| `title`    | string     | drawn at the top of the dock card                        |
| `lines[]`  | string[]   | Tier-1 text body                                         |
| `draw[]`   | DrawCmd[]  | Tier-2 immediate-mode primitives (optional, `api ≥ 2`)   |
| `canvas_h` | number     | Tier-2 canvas height in logical px (optional, 8–360)     |

A Tier-1 widget sets `lines`. A Tier-2 widget sets `draw` (and usually
`canvas_h`); termie paints the canvas under the title, then any `lines` below it.
Unknown widget fields are ignored, so a v1 host silently shows the Tier-1 body.

### Tier-2 immediate-mode drawing (`api_version` ≥ 2)

`draw` is a list of primitives painted into the widget's canvas box. Coordinates
are **normalized 0..1** within that box, so a plugin is independent of DPI and
window size and can never paint outside its own widget. Up to 256 primitives are
drawn per widget; extras are dropped.

| primitive | fields                              |
|-----------|-------------------------------------|
| `rect`    | `x`, `y`, `w`, `h`, `color`         |
| `text`    | `x`, `y`, `text`, `color`           |

`color` is a palette role — `paper`, `text`, `mute`, `rule`, `rule2`, `ink`,
`ink0`, `ink3`, `ink4`, `accent` — or a `"#rrggbb"` / `"#rgb"` hex string. Palette
roles follow the user's theme; an unrecognized spec falls back to the body color.
A meter is a track `rect` plus a narrower fill `rect`; a bar chart is a row of
rects; labels are `text`.

### Minimal exchange

```
<- {"t":"hello","api_version":2,"permissions":[]}
-> {"t":"ready","name":"tamagotchi","api_version":2}
-> {"t":"declare_widget","widget":{"id":"pet","title":"Tama","lines":[]}}
-> {"t":"update_widget","widget":{"id":"pet","title":"Tama","canvas_h":76,"draw":[
     {"t":"text","x":0.0,"y":0.0,"text":">  w  <","color":"paper"},
     {"t":"rect","x":0.3,"y":0.3,"w":0.7,"h":0.14,"color":"ink3"},
     {"t":"rect","x":0.3,"y":0.3,"w":0.56,"h":0.14,"color":"#83a06d"}
   ]}}
```

A plugin should read lines from stdin in a loop and exit cleanly when stdin
closes or it receives `shutdown`. The reference `tamagotchi` plugin sends the
Tier-1 body on a v1 host and upgrades to graphical meters on a v2 host — see
`plugins/tamagotchi/main.rs`.
