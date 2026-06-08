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

pub use manifest::{id_is_safe, Manifest, KNOWN_PERMISSIONS};
pub use proto::{DrawCmd, HostEvent, PluginCmd, API_VERSION};

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;

/// a message surfaced from a plugin to the event loop (mirrors PtyMsg)
pub enum PluginMsg {
    Cmd(PluginCmd),
    Exited,
}

/// the OS process behind a plugin: either a normal child or, when sandboxing is
/// enabled, an appcontainer-confined process spawned through `sandbox`
enum Proc {
    Std(Child),
    #[cfg(windows)]
    Sandbox(sandbox::Sandboxed),
}

impl Proc {
    fn kill(&mut self) {
        match self {
            Proc::Std(c) => {
                let _ = c.kill();
            }
            #[cfg(windows)]
            Proc::Sandbox(s) => s.kill(),
        }
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
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let stdout: Box<dyn Read + Send> = Box::new(child.stdout.take().expect("piped stdout"));
        let writer = child.stdin.take().map(|w| Box::new(w) as Box<dyn Write + Send>);
        Self::start_reader(id, stdout, on_msg);
        Ok(Plugin { proc: Proc::Std(child), writer })
    }

    /// spawn `program args...` as a plugin confined to a windows appcontainer
    /// named `moniker`, with `dir` as its working dir and granted dir, allowing
    /// outbound network only when `net` is set
    #[cfg(windows)]
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
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        on_msg(PluginMsg::Exited);
                        break;
                    }
                    Ok(_) => {
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
}
