---
name: Bug report
about: Something in termie misbehaves
title: ''
labels: bug
---

**What happened**
A clear description of the bug, and what you expected instead.

**Repro**
Steps to reproduce. If it's a rendering or escape-sequence issue, the exact
output or sequence that triggers it is the most useful thing you can include
(for example a `printf '\e[...'` line, or the program and command that was
running when it happened).

**Environment**
- termie version (the About panel, or the commit you built from):
- Windows version:
- Shell (pwsh / powershell / cmd / wsl):
- Program running in the pane, if relevant (vim, a pager, a TUI, …):
- GPU / driver, if it looks like a rendering issue:

**Logs / screenshots**
Anything else that helps — a screenshot, or a `--termview` dump if you built in
debug.
