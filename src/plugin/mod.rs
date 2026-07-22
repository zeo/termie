//! plugin host: spawns a plugin as a separate OS process and talks to it over
//! newline-delimited json on stdin/stdout (see `proto`). plugins run out of
//! process so they can be arbitrarily heavy in any language without adding any
//! weight to termie's core — the host is just a supervised message pipe.
//!
//! security note: a subprocess gives crash isolation, NOT a privilege sandbox.
//! a plugin runs with the user's rights; trust is managed by a curated store +
//! the install-time permission display, not by this boundary

pub mod json;
mod manifest;
pub mod market;
mod proto;
#[cfg(windows)]
pub mod sandbox;
#[cfg(unix)]
#[path = "sandbox_unix.rs"]
pub mod sandbox;

pub use manifest::{id_is_safe, Manifest, KNOWN_PERMISSIONS};
pub use proto::{DrawCmd, HostEvent, PluginCmd, API_VERSION};

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;

/// a message surfaced from a plugin to the event loop (mirrors PtyMsg)
#[derive(Debug)]
pub enum PluginMsg {
    Cmd(PluginCmd),
    Exited,
}

/// the OS process behind a plugin: either a normal child or, when sandboxing is
/// enabled, an appcontainer-confined process spawned through `sandbox`
enum Proc {
    Std(Child),
    Sandbox(sandbox::Sandboxed),
}

impl Proc {
    fn kill(&mut self) {
        match self {
            Proc::Std(c) => {
                let _ = c.kill();
            }
            Proc::Sandbox(s) => s.kill(),
        }
    }
}

fn discard_line(reader: &mut impl BufRead) -> std::io::Result<()> {
    loop {
        let bytes = reader.fill_buf()?;
        let Some(newline) = bytes.iter().position(|&byte| byte == b'\n') else {
            let len = bytes.len();
            if len == 0 {
                return Ok(());
            }
            reader.consume(len);
            continue;
        };
        reader.consume(newline + 1);
        return Ok(());
    }
}

/// a running plugin process. dropping or calling kill() stops it. the App
/// tracks plugins by their Vec index, so no id is stored here; `id` is only
/// used to label this plugin's log lines from the reader thread
pub struct Plugin {
    proc: Proc,
    writer: Option<Box<dyn Write + Send>>,
}

impl Plugin {
    /// spawn `program args...` as a plugin and wire its stdout to `on_msg`
    /// (called on a reader thread, once per ndjson line). stderr is inherited
    /// for now so plugin logs are visible in debug runs
    pub fn spawn(
        id: impl Into<String>,
        program: &str,
        args: &[String],
        on_msg: impl Fn(PluginMsg) + Send + 'static,
    ) -> std::io::Result<Plugin> {
        let id = id.into();
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // a gui host spawning a console-subsystem plugin would otherwise pop a
        // console window; suppress it (the sandbox path already does)
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn()?;

        let stdout: Box<dyn Read + Send> = Box::new(child.stdout.take().expect("piped stdout"));
        let writer = child.stdin.take().map(|w| Box::new(w) as Box<dyn Write + Send>);
        Self::start_reader(id, stdout, on_msg);
        Ok(Plugin { proc: Proc::Std(child), writer })
    }

    /// spawn `program args...` as a plugin confined to the OS sandbox (a
    /// windows appcontainer / a linux bwrap jail) named `moniker`, with `dir`
    /// as its working dir and granted dir, allowing outbound network only
    /// when `net` is set
    pub fn spawn_sandboxed(
        id: impl Into<String>,
        moniker: &str,
        program: &std::path::Path,
        args: &[String],
        dir: &std::path::Path,
        net: bool,
        on_msg: impl Fn(PluginMsg) + Send + 'static,
    ) -> std::io::Result<Plugin> {
        let id = id.into();
        let mut sb = sandbox::spawn(moniker, program, args, dir, net)?;
        let stdout: Box<dyn Read + Send> = Box::new(
            sb.take_stdout()
                .ok_or_else(|| std::io::Error::other("sandbox stdout missing"))?,
        );
        let writer = sb
            .take_stdin()
            .map(|w| Box::new(w) as Box<dyn Write + Send>);
        Self::start_reader(id, stdout, on_msg);
        Ok(Plugin { proc: Proc::Sandbox(sb), writer })
    }

    /// reader thread: parse each line, forward to the event loop. a line that
    /// isn't valid json is dropped (logged, labeled by id) rather than killing
    /// the plugin
    fn start_reader(
        id: String,
        stdout: Box<dyn Read + Send>,
        on_msg: impl Fn(PluginMsg) + Send + 'static,
    ) {
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            const MAX_LINE_LIMIT: u64 = 262_144; // 256 kb
            loop {
                line.clear();
                let mut chunk = reader.by_ref().take(MAX_LINE_LIMIT);
                match chunk.read_line(&mut line) {
                    Ok(0) => {
                        on_msg(PluginMsg::Exited);
                        break;
                    }
                    Ok(n) => {
                        if n as u64 >= MAX_LINE_LIMIT && !line.ends_with('\n') {
                            log::warn!("plugin {id}: line exceeds maximum length limit, discarding line");
                            if discard_line(&mut reader).is_err() {
                                on_msg(PluginMsg::Exited);
                                break;
                            }
                            continue;
                        }
                        if line.trim().is_empty() {
                            continue;
                        }
                        match PluginCmd::from_line(&line) {
                            Some(cmd) => on_msg(PluginMsg::Cmd(cmd)),
                            None => log::warn!("plugin {id}: bad json line dropped"),
                        }
                    }
                    Err(_) => {
                        on_msg(PluginMsg::Exited);
                        break;
                    }
                }
            }
        });
    }

    /// send a host event to the plugin (newline-delimited). best-effort: a write
    /// error just means the plugin went away and the reader will report Exited
    pub fn send(&mut self, ev: &HostEvent) {
        if let Some(w) = self.writer.as_mut() {
            let mut line = ev.to_line();
            line.push('\n');
            let _ = w.write_all(line.as_bytes());
            let _ = w.flush();
        }
    }

    pub fn kill(&mut self) {
        // closing stdin lets a well-behaved plugin exit cleanly; then ensure it
        let _ = self.writer.take();
        self.proc.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_event_lines_end_clean() {
        let ev = HostEvent::Bell { pane: 3 };
        let line = ev.to_line();
        assert!(!line.contains('\n'));
        assert!(crate::plugin::json::Json::parse(&line).is_some());
    }

    #[test]
    fn start_reader_drops_overlarge_lines() {
        use std::sync::mpsc;
        use std::time::Duration;

        let mut payload = vec![b'a'; 300_000];
        payload.push(b'\n');
        payload.extend_from_slice(r#"{"t":"notify","text":"hello"}"#.as_bytes());
        payload.push(b'\n');

        let (tx, rx) = mpsc::channel();
        let stdout: Box<dyn Read + Send> = Box::new(std::io::Cursor::new(payload));

        Plugin::start_reader(
            "test_plugin".to_string(),
            stdout,
            move |msg| {
                let _ = tx.send(msg);
            },
        );

        let first = rx.recv_timeout(Duration::from_millis(500)).expect("recv first");
        match first {
            PluginMsg::Cmd(PluginCmd::Notify { text }) => assert_eq!(text, "hello"),
            other => panic!("expected Notify, got {:?}", other),
        }

        let second = rx.recv_timeout(Duration::from_millis(500)).expect("recv second");
        assert!(matches!(second, PluginMsg::Exited));
    }

    #[test]
    fn discarding_a_line_preserves_the_next_line() {
        let mut reader = BufReader::new(std::io::Cursor::new(b"discard this\nkeep this\n"));
        discard_line(&mut reader).expect("discard line");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read next line");
        assert_eq!(line, "keep this\n");
    }
}
