# termie examples

This directory contains minimal reference examples for writing termie plugins
and configuring window effects.

- `hello-plugin/` — the smallest possible plugin: announces `ready`, declares
  a text widget, then updates it every second with a counter and the current
  focus/tab. Shows the Newline-Delimited JSON (NDJSON) wire protocol without any
  external dependency. Any language that can read stdin / write stdout can do
  the same; the reference here is Python for readability.

  To try it, copy the folder into the plugins directory (`%APPDATA%\termie\plugins\`
  on Windows, `~/.local/share/termie/plugins/` on Linux) without renaming it:
  the manifest `id` must match the folder name. Restart termie and its
  widget shows up in the side dock. A relative `cmd` resolves inside the plugin folder,
  so the manifest names the interpreter by absolute path. `/usr/bin/python3`
  works as shipped on Linux. On Windows, set `cmd` to your `python.exe`; the
  sandbox can only run one installed for all users under `C:\Program Files`,
  so either install Python that way or set `plugin_sandbox=off` while you
  develop. JSON needs the backslashes doubled:
  `"cmd": "C:\\Program Files\\Python313\\python.exe"`.
- `acrylic.md` — notes on the `acrylic=true` config key (Win11 Mica backdrop).

See also `%APPDATA%\termie\config` keys documented in README.md's Configuration
section, and the plugin protocol docs in `src/plugin/proto.rs`.
