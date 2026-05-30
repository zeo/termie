# termie plugins

Plugins extend termie **without bloating the core**. A plugin is a separate OS
process that termie launches and talks to over a tiny line protocol. Because
plugins run out of process, a plugin can be written in any language and be as
heavy as it likes — the core stays lean and fast, and a crashing plugin can't
take termie down.

See `docs/plugin-system-plan.md` for the full design and rationale.

> Status: Phase 1 (host + protocol spine) is implemented. Tier-1 widget
> rendering, the plugin bus, and the in-app marketplace land in later phases.
> The protocol below is the v1 contract (`api_version` 1).

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
  "api_version": 1,
  "entry": { "cmd": "tamagotchi.exe", "args": [] },
  "permissions": []
}
```

- `id` — unique; defaults to the directory name if omitted.
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

| `t`              | fields                          |
|------------------|---------------------------------|
| `hello`          | `api_version`, `permissions[]`  | sent once on startup (handshake) |
| `focus_changed`  | `pane`                          |
| `tab_changed`    | `tab`                           |
| `cwd_changed`    | `cwd`                           |
| `bell`           | `pane`                          |
| `widget_clicked` | `id`                            |
| `message`        | `from`, `topic`, `body`         | from another plugin via the bus |
| `shutdown`       | —                               |

### Plugin → termie commands

| `t`              | fields                          |
|------------------|---------------------------------|
| `ready`          | `name`, `api_version`           | announce yourself after `hello` |
| `declare_widget` | `widget` (`id`, `title`, `lines[]`) |
| `update_widget`  | `widget`                        |
| `notify`         | `text`                          |
| `write_pty`      | `data`                          | requires the `write_pty` permission |
| `publish`        | `topic`, `body`                 | publish to the bus |
| `subscribe`      | `topic` (`"*"` = all)           |

### Minimal exchange

```
<- {"t":"hello","api_version":1,"permissions":[]}
-> {"t":"ready","name":"tamagotchi","api_version":1}
-> {"t":"declare_widget","widget":{"id":"pet","title":"Tama","lines":["happy","hunger 80%"]}}
<- {"t":"bell","pane":0}
-> {"t":"update_widget","widget":{"id":"pet","title":"Tama","lines":["startled!","hunger 80%"]}}
```

A plugin should read lines from stdin in a loop and exit cleanly when stdin
closes or it receives `shutdown`.
