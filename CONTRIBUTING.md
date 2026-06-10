# Contributing to termie

termie is early but daily-usable, and Windows-first. Bug reports, clear repros, and focused pull requests are all welcome.

## Building

You need the stable [Rust toolchain](https://rustup.rs/).

```powershell
cargo build              # debug
cargo test               # unit + golden tests
cargo test -- --ignored  # live PTY integration tests (spawn a real shell; local only)
cargo clippy --all-targets
cargo build --release    # optimized, ~7.6 MB
```

The `--ignored` tests spawn a real shell through the pty and aren't run in CI (they're timing-sensitive); run them locally when changing the pty or terminal-response path.

Plugins live in their own repo — [`lintowe/termie-plugins`](https://github.com/lintowe/termie-plugins) — which is also where you add or change one (see its `CONTRIBUTING.md`).

## House style

- **No rustfmt.** termie keeps its own source layout, and CI does not run `cargo fmt`. Don't reformat files you aren't otherwise changing — match the surrounding style.
- **Keep the build warning-clean.** `cargo clippy --all-targets` passes with no warnings today. Keep it that way.
- Prefer the standard library and existing dependencies over pulling in new ones; a new production dependency needs a real reason.
- Comments explain why, not what — skip the ones that just restate the code.

## Terminal and rendering changes

These are verifiable without opening a window — everything runs through the real parser, grid, and glyph atlas:

```powershell
cargo run -- --termview --scenario sgr                  # dump the grid + state as text
cargo run -- --termview --seq "\e[31mhi"                # feed an escape sequence
cargo run -- --termview --scenario wrap --png out.png   # render the scene to an image
```

`cargo test golden` diffs a set of fixed scenarios against checked-in snapshots in `tests/golden/`. If a change *intentionally* alters rendering, re-bless and review the diff before committing:

```powershell
$env:BLESS=1; cargo test golden; $env:BLESS=$null
git diff tests/golden    # read exactly what changed
```

A new terminal feature should land with a golden scenario or a unit test that locks its behavior.

## Pull requests

- Keep commits small and focused, with a clear summary line. The history uses short prefixes (`fix:`, `perf:`, `feat:`, `test:`, `polish:`) — follow suit.
- Make sure `cargo test` and `cargo clippy --all-targets` pass. CI runs build + tests, clippy, and a `cargo-audit` scan on every push and pull request.
- Say what changed and why, and link any issue the PR closes.

## License

By contributing, you agree that your contributions are dual-licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE) — the same terms as the project.
