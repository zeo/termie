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
use std::sync::mpsc::{self, SyncSender};
use std::thread;

use market::ProcessTree;

const PLUGIN_WRITE_QUEUE_CAP: usize = 64;
const MAX_HOST_EVENT_BYTES: usize = 64 * 1024;

fn queue_host_event(tx: &SyncSender<Vec<u8>>, line: Vec<u8>) -> bool {
    line.len() <= MAX_HOST_EVENT_BYTES && tx.try_send(line).is_ok()
}

/// a message surfaced from a plugin to the event loop (mirrors PtyMsg)
#[derive(Debug)]
pub enum PluginMsg {
    Cmd(PluginCmd),
    Exited,
}

/// the OS process behind a plugin: either a normal child or, when sandboxing is
/// enabled, an appcontainer-confined process spawned through `sandbox`
enum Proc {
    Std { child: Child, tree: ProcessTree },
    Sandbox(sandbox::Sandboxed),
    #[cfg(test)]
    Tracking(std::sync::Arc<std::sync::atomic::AtomicBool>),
}

impl Proc {
    fn kill(&mut self) {
        match self {
            Proc::Std { child, tree } => {
                #[cfg(unix)]
                unsafe {
                    let _ = libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                }
                tree.terminate();
                let _ = child.kill();
                let _ = child.wait();
            }
            Proc::Sandbox(s) => s.kill(),
            #[cfg(test)]
            Proc::Tracking(killed) => {
                killed.store(true, std::sync::atomic::Ordering::Release);
            }
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
    writer_tx: Option<SyncSender<Vec<u8>>>,
}

impl Drop for Plugin {
    fn drop(&mut self) {
        self.kill();
    }
}

impl Plugin {
    /// spawn `program args...` as a plugin, in `dir` when given, and wire its
    /// stdout to `on_msg` (called on a reader thread, once per ndjson line).
    /// stderr is inherited for now so plugin logs are visible in debug runs
    pub fn spawn(
        id: impl Into<String>,
        program: &str,
        args: &[String],
        dir: Option<&std::path::Path>,
        on_msg: impl Fn(PluginMsg) + Send + 'static,
    ) -> std::io::Result<Plugin> {
        let id = id.into();
        let mut cmd = Command::new(program);
        if let Some(dir) = dir {
            cmd.current_dir(dir);
        }
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
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
        }
        let mut child = cmd.spawn()?;

        let stdout: Box<dyn Read + Send> = Box::new(child.stdout.take().expect("piped stdout"));
        let writer_tx = child.stdin.take().map(|w| Self::start_writer(Box::new(w)));
        Self::start_reader(id, stdout, on_msg);
        let tree = ProcessTree::attach(&child);
        Ok(Plugin { proc: Proc::Std { child, tree }, writer_tx })
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
        let writer_tx = sb
            .take_stdin()
            .map(|w| Self::start_writer(Box::new(w)));
        Self::start_reader(id, stdout, on_msg);
        Ok(Plugin { proc: Proc::Sandbox(sb), writer_tx })
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

    fn start_writer(mut writer: Box<dyn Write + Send>) -> SyncSender<Vec<u8>> {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(PLUGIN_WRITE_QUEUE_CAP);
        thread::spawn(move || {
            while let Ok(line) = rx.recv() {
                if writer.write_all(&line).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });
        tx
    }

    /// queue a host event for the plugin without blocking the ui thread
    pub fn send(&mut self, ev: &HostEvent) {
        if let Some(tx) = self.writer_tx.as_ref() {
            let mut line = ev.to_line();
            line.push('\n');
            let _ = queue_host_event(tx, line.into_bytes());
        }
    }

    pub fn kill(&mut self) {
        // closing stdin lets a well-behaved plugin exit cleanly; then ensure it
        let _ = self.writer_tx.take();
        self.proc.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_plugin_terminates_its_process() {
        let killed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let plugin = Plugin {
            proc: Proc::Tracking(killed.clone()),
            writer_tx: None,
        };
        drop(plugin);
        assert!(killed.load(std::sync::atomic::Ordering::Acquire));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn killing_standard_plugin_reaps_its_process_group() {
        use std::time::{Duration, Instant};

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let marker = std::env::temp_dir().join(format!("termie-plugin-child-{}-{nonce}", std::process::id()));
        let command = format!("sleep 60 & printf %s \"$!\" > '{}' && wait", marker.display());
        let mut plugin = Plugin::spawn("test", "/bin/sh", &["-c".into(), command], None, |_| {}).expect("spawn plugin");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let child_pid: u32 = std::fs::read_to_string(&marker).expect("child pid").parse().expect("numeric child pid");
        plugin.kill();
        let deadline = Instant::now() + Duration::from_secs(1);
        while std::path::Path::new(&format!("/proc/{child_pid}")).exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(marker);
        assert!(!std::path::Path::new(&format!("/proc/{child_pid}")).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unsandboxed_plugin_runs_in_its_install_dir() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hello-plugin");
        if !std::path::Path::new("/usr/bin/python3").exists() {
            eprintln!("skipping: /usr/bin/python3 is not installed");
            return;
        }
        let (tx, rx) = mpsc::channel();
        let _plugin = Plugin::spawn("hello-plugin", "/usr/bin/python3", &["plugin.py".into()], Some(&dir), move |msg| {
            let _ = tx.send(matches!(msg, PluginMsg::Cmd(PluginCmd::Ready { .. })));
        })
        .expect("spawn example plugin");
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(10)), Ok(true));
    }

    #[test]
    fn host_event_queue_is_bounded() {
        let (tx, _rx) = mpsc::sync_channel(1);
        assert!(queue_host_event(&tx, vec![b'x'; MAX_HOST_EVENT_BYTES]));
        assert!(!queue_host_event(&tx, b"next".to_vec()));
        let (tx, _rx) = mpsc::sync_channel(1);
        assert!(!queue_host_event(&tx, vec![b'x'; MAX_HOST_EVENT_BYTES + 1]));
    }

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
