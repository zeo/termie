# Security policy

## Supported versions

termie is in early `0.x` development. Security fixes land on `main` and ship in the next release; there are no separate maintenance branches yet.

## Reporting a vulnerability

Please report security issues privately — don't open a public issue for them.

Use GitHub's private vulnerability reporting on this repository: the **Security** tab, then **Report a vulnerability**. That opens a private advisory with the maintainers. Include what you found, the impact, and a repro if you have one. You'll get an acknowledgement and a fix or an assessment as fast as is practical.

## Scope notes

These are the deliberate boundaries; a report that shows a way around one is exactly what's useful:

- termie runs each plugin as a **separate OS process** over a line-delimited JSON protocol and confines it in a Windows AppContainer or Linux bubblewrap jail by default. Sensitive capabilities (`read_output`, `write_pty`, `network`) are off unless explicitly granted.
- The plugin JSON parser bounds its recursion depth, and the kitty-graphics scanner caps both the buffered escape sequence and the reassembled image size, so a hostile or garbled stream can't grow memory without bound.
- termie refuses OSC 52 clipboard reads. Clipboard writes are disabled unless `osc52=true` is set in the config.
- OSC 7 accepts local absolute paths only. Remote authorities and Windows UNC paths are rejected, and terminal-provided paths are never probed for repository metadata.
