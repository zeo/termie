use std::env;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::thread;

use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

// a prompt hook that emits OSC-133;A (prompt mark, drives ctrl+up/down jump
// nav) and OSC-7 (cwd) before the prompt text. it wraps whatever prompt is
// already defined — the pwsh default, or the profile's starship/oh-my-posh
// when load_profile is on — instead of replacing it. single-quoted pwsh +
// string concatenation so the command line carries no double quotes
const PWSH_PROMPT_HOOK: &str = r#"$global:__termie_prompt = $function:prompt; function prompt { $p=$PWD.ProviderPath; [char]27+']133;A'+[char]27+'\'+[char]27+']7;file:///'+($p -replace '\\','/')+[char]27+'\'+(& $global:__termie_prompt) }"#;

/// which shell a new pane should launch
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ShellKind {
    #[default]
    Auto,
    Pwsh,
    PowerShell,
    Cmd,
    Wsl,
}

impl ShellKind {
    pub fn next(self) -> Self {
        match self {
            ShellKind::Auto => ShellKind::Pwsh,
            ShellKind::Pwsh => ShellKind::PowerShell,
            ShellKind::PowerShell => ShellKind::Cmd,
            ShellKind::Cmd => ShellKind::Wsl,
            ShellKind::Wsl => ShellKind::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ShellKind::Auto => "auto",
            ShellKind::Pwsh => "pwsh",
            ShellKind::PowerShell => "powershell",
            ShellKind::Cmd => "cmd",
            ShellKind::Wsl => "wsl",
        }
    }

    pub fn from_label(s: &str) -> Self {
        match s {
            "pwsh" => ShellKind::Pwsh,
            "powershell" => ShellKind::PowerShell,
            "cmd" => ShellKind::Cmd,
            "wsl" => ShellKind::Wsl,
            _ => ShellKind::Auto,
        }
    }
}

pub enum PtyMsg {
    Output(Vec<u8>),
    Exited,
}

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    // input goes through a dedicated writer thread: a child that stops reading
    // (paused pager, stopped process) fills the ConPTY input pipe, and a direct
    // write_all from the ui thread would freeze the whole window on a big paste
    writer_tx: std::sync::mpsc::Sender<Vec<u8>>,
    child: Box<dyn Child + Send + Sync>,
    // reader is parked until start_reader() spawns the output thread; this lets
    // a pane be built off the main thread and only start emitting once registered
    reader: Option<Box<dyn std::io::Read + Send>>,
}

impl Pty {
    /// create the pty + child process (the slow part — safe to call off-thread).
    /// the output thread isn't started until start_reader().
    pub fn spawn(
        rows: u16,
        cols: u16,
        shell: ShellKind,
        load_profile: bool,
        cwd: Option<&str>,
        command: Option<&[String]>,
        wsl_distro: Option<&str>,
        term_program: &str,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<Pty> {
        let pty_system = native_pty_system();
        // pass real cell geometry when known so ConPTY (and anything that asks
        // the console for pixel size) sees a honest window, not 0×0
        let cols = cols.max(1);
        let rows = rows.max(1);
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: pixel_width.saturating_mul(cols),
            pixel_height: pixel_height.saturating_mul(rows),
        })?;

        // an explicit command (from the cli or a context-menu verb) runs directly
        // instead of a login shell, so none of the shell banner/prompt-hook
        // injection applies; otherwise launch the configured shell
        let mut cmd = match command.filter(|a| !a.is_empty()) {
            Some(argv) => {
                let mut c = CommandBuilder::new(&argv[0]);
                for a in &argv[1..] {
                    c.arg(a.as_str());
                }
                c
            }
            None => {
                let shell = resolve_shell_cached(shell);
                // suppress the banner (and the profile unless asked) for a fast
                // prompt, and inject an OSC-7 hook so termie learns the cwd
                let lower = shell.to_ascii_lowercase();
                let mut c = CommandBuilder::new(&shell);
                if lower.ends_with("pwsh.exe") || lower.ends_with("powershell.exe") {
                    c.arg("-NoLogo");
                    if !load_profile {
                        c.arg("-NoProfile");
                    }
                    c.arg("-NoExit");
                    c.arg("-Command");
                    c.arg(PWSH_PROMPT_HOOK);
                }
                if lower.ends_with("wsl.exe") {
                    // launch a specific distro when one is configured (else the
                    // wsl default), and forward the terminal env in so colors and
                    // identity reach programs running inside wsl
                    if let Some(d) = wsl_distro {
                        c.arg("-d");
                        c.arg(d);
                    }
                    c.env(
                        "WSLENV",
                        "TERM/u:COLORTERM/u:TERM_PROGRAM/u:TERM_PROGRAM_VERSION/u:TERMIE/u",
                    );
                }
                c
            }
        };
        // start in the requested directory (a new tab/split in the focused repo),
        // falling back to home if it's unset or no longer a valid directory
        if let Some(dir) = cwd.filter(|d| std::path::Path::new(*d).is_dir()) {
            cmd.cwd(dir);
        } else if let Some(home) = env::var_os("USERPROFILE") {
            cmd.cwd(home);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERMIE", "1");
        // identify as termie by default. spoofing another host (e.g. ghostty)
        // makes allowlist-only apps enable every progressive feature that host
        // advertises — including ones that misbehave through ConPTY and then
        // dump raw mouse/keyboard sequences into TUI input buffers. real
        // capability is negotiated (kitty keyboard CSI, XTVERSION, DA, tcap);
        // set term_program=ghostty in config only when an allowlist-only app
        // needs the name and you accept the tradeoff
        let tp = if term_program.is_empty() {
            "termie"
        } else {
            term_program
        };
        cmd.env("TERM_PROGRAM", tp);
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        // trim pwsh/.NET startup work: skip the background update check + telemetry
        cmd.env("POWERSHELL_UPDATECHECK", "Off");
        cmd.env("POWERSHELL_TELEMETRY_OPTOUT", "1");
        cmd.env("DOTNET_CLI_TELEMETRY_OPTOUT", "1");
        cmd.env("DOTNET_NOLOGO", "1");

        let child = pair.slave.spawn_command(cmd)?;
        // close the slave side in the parent so EOF propagates on child exit
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let (writer_tx, writer_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            // exits when the Pty drops (channel closes) or the pipe breaks
            while let Ok(chunk) = writer_rx.recv() {
                if writer.write_all(&chunk).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });

        Ok(Pty {
            master: pair.master,
            writer_tx,
            child,
            reader: Some(reader),
        })
    }

    /// start the output thread; call once, after the pane is registered so no
    /// early output is dropped
    pub fn start_reader(&mut self, on_event: impl Fn(PtyMsg) + Send + 'static) {
        let Some(mut reader) = self.reader.take() else {
            return;
        };
        // optional raw-output capture for debugging a rendering issue: set
        // TERMIE_CAPTURE=<path> to append every byte the shell emits, then replay
        // it through `termie --termview --file <path>` to reproduce exactly
        let mut capture = std::env::var_os("TERMIE_CAPTURE")
            .and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
        thread::spawn(move || {
            // a larger read means fewer, fatter UserEvent::Pty hops under heavy
            // streaming, cutting the fixed per-event handler cost (find_pane walk,
            // cross-thread send); the Vec is sized to bytes actually read
            let mut buf = [0u8; 32768];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        on_event(PtyMsg::Exited);
                        break;
                    }
                    Ok(n) => {
                        if let Some(f) = capture.as_mut() {
                            let _ = f.write_all(&buf[..n]);
                        }
                        on_event(PtyMsg::Output(buf[..n].to_vec()));
                    }
                    Err(_) => {
                        on_event(PtyMsg::Exited);
                        break;
                    }
                }
            }
        });
    }

    /// queue bytes for the child; the writer thread does the blocking write, so
    /// input order is preserved and the ui thread never blocks on the pipe
    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer_tx.send(bytes.to_vec());
    }

    pub fn resize(&mut self, rows: u16, cols: u16, cell_w: u16, cell_h: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: cell_w.saturating_mul(cols),
            pixel_height: cell_h.saturating_mul(rows),
        });
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

/// resolved shell path memoized per ShellKind: %PATH% doesn't change within a
/// session, so walking it once per kind avoids repeated disk I/O on every pool
/// spawn (build_pane runs on worker threads, hence the lock)
fn resolve_shell_cached(kind: ShellKind) -> String {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<Vec<(ShellKind, String)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(c) = cache.lock()
        && let Some((_, p)) = c.iter().find(|(k, _)| *k == kind) {
            return p.clone();
        }
    let resolved = resolve_shell(kind);
    if let Ok(mut c) = cache.lock()
        && !c.iter().any(|(k, _)| *k == kind) {
            c.push((kind, resolved.clone()));
        }
    resolved
}

fn find_in_path(exe: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// resolve a shell kind to an executable path, falling back to whatever is
/// available (pwsh → powershell → cmd) for Auto or when the request isn't found
fn resolve_shell(kind: ShellKind) -> String {
    let auto = || {
        find_in_path("pwsh.exe")
            .or_else(|| find_in_path("powershell.exe"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cmd.exe".to_string())
    };
    match kind {
        ShellKind::Auto => auto(),
        ShellKind::Pwsh => find_in_path("pwsh.exe")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(auto),
        ShellKind::PowerShell => find_in_path("powershell.exe")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(auto),
        ShellKind::Cmd => find_in_path("cmd.exe")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cmd.exe".to_string()),
        ShellKind::Wsl => find_in_path("wsl.exe")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "wsl.exe".to_string()),
    }
}

#[cfg(test)]
impl Pty {
    /// a no-op pty for tests: build a Pane without spawning a real shell
    pub(crate) fn null() -> Pty {
        // the receiver is dropped, so writes are discarded without a thread
        let (writer_tx, _) = std::sync::mpsc::channel();
        Pty {
            master: Box::new(null_pty::NullMaster),
            writer_tx,
            child: Box::new(null_pty::NullChild),
            reader: None,
        }
    }
}

// test-only null pty: lets a Pane be built without spawning a shell, so the
// pane-tree and layout logic can be exercised headlessly. every operation is a
// no-op (kill/resize/write do nothing; reads return EOF)
#[cfg(test)]
mod null_pty {
    use super::*;
    use portable_pty::{ChildKiller, ExitStatus};
    use std::io::Result as IoResult;

    #[derive(Debug)]
    pub struct NullKiller;
    impl ChildKiller for NullKiller {
        fn kill(&mut self) -> IoResult<()> {
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(NullKiller)
        }
    }

    #[derive(Debug)]
    pub struct NullChild;
    impl ChildKiller for NullChild {
        fn kill(&mut self) -> IoResult<()> {
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(NullKiller)
        }
    }
    impl Child for NullChild {
        fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
            Ok(None)
        }
        fn wait(&mut self) -> IoResult<ExitStatus> {
            Ok(ExitStatus::with_exit_code(0))
        }
        fn process_id(&self) -> Option<u32> {
            None
        }
        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    pub struct NullMaster;
    impl MasterPty for NullMaster {
        fn resize(&self, _: PtySize) -> anyhow::Result<()> {
            Ok(())
        }
        fn get_size(&self) -> anyhow::Result<PtySize> {
            Ok(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        }
        fn try_clone_reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
            Ok(Box::new(std::io::empty()))
        }
        fn take_writer(&self) -> anyhow::Result<Box<dyn std::io::Write + Send>> {
            Ok(Box::new(std::io::sink()))
        }
        #[cfg(unix)]
        fn process_group_leader(&self) -> Option<libc::pid_t> {
            None
        }
        #[cfg(unix)]
        fn as_raw_fd(&self) -> Option<portable_pty::unix::RawFd> {
            None
        }
        #[cfg(unix)]
        fn tty_name(&self) -> Option<std::path::PathBuf> {
            None
        }
    }
}

// live integration tests: spawn a real shell through the pty and exercise the
// full spawn -> read -> parse -> reply -> render path end-to-end. pty output is
// fed through a real Terminal so DSR/DA queries (ConPTY's startup `ESC[6n`,
// which gates the child's output until it's answered) are replied to, exactly
// as the app does. spawning real processes is timing-sensitive, so these are
// #[ignore]d to keep them out of CI (the release plan flagged them as flaky);
// run locally with `cargo test -- --ignored`
#[cfg(all(test, windows))]
mod live_tests {
    use super::*;
    use crate::term::Terminal;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use vte::Parser;

    fn grid_text(term: &Terminal) -> String {
        term.grid
            .lines
            .iter()
            .map(|line| line.iter().map(|c| c.c).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // feed pty output into a Terminal until `needle` renders into the grid or the
    // timeout passes, writing the terminal's query replies (DSR/DA) back to the
    // pty so ConPTY proceeds. returns the final grid text.
    fn pump_until(
        pty: &mut Pty,
        rx: &mpsc::Receiver<PtyMsg>,
        rows: usize,
        cols: usize,
        needle: &str,
        timeout: Duration,
    ) -> String {
        let mut term = Terminal::new(rows, cols);
        let mut parser = Parser::new();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(PtyMsg::Output(b)) => {
                    parser.advance(&mut term, &b);
                    if !term.responses.is_empty() {
                        let reply = std::mem::take(&mut term.responses);
                        pty.write(&reply);
                    }
                    if grid_text(&term).contains(needle) {
                        break;
                    }
                }
                Ok(PtyMsg::Exited) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        grid_text(&term)
    }

    fn reader_channel(pty: &mut Pty) -> mpsc::Receiver<PtyMsg> {
        let (tx, rx) = mpsc::channel();
        pty.start_reader(move |m| {
            let _ = tx.send(m);
        });
        rx
    }

    #[test]
    #[ignore = "spawns a real shell; run locally with `cargo test -- --ignored`"]
    fn pty_runs_a_command_and_renders_output() {
        let argv = ["cmd.exe", "/c", "echo termie-itest-OK"].map(String::from);
        let mut pty =
            Pty::spawn(24, 80, ShellKind::Cmd, false, None, Some(&argv[..]), None, "termie", 0, 0)
                .expect("spawn pty");
        let rx = reader_channel(&mut pty);
        let grid = pump_until(&mut pty, &rx, 24, 80, "termie-itest-OK", Duration::from_secs(15));
        pty.kill();
        assert!(grid.contains("termie-itest-OK"), "command output not rendered; grid: {grid:?}");
    }

    #[test]
    #[ignore = "spawns a real shell; run locally with `cargo test -- --ignored`"]
    fn pty_echoes_typed_input() {
        let mut pty =
            Pty::spawn(24, 80, ShellKind::Cmd, false, None, None, None, "termie", 0, 0)
                .expect("spawn pty");
        let rx = reader_channel(&mut pty);
        // let the shell come up (answering its startup queries), then type a command
        std::thread::sleep(Duration::from_millis(700));
        pty.write(b"echo termie-input-OK\r\n");
        let grid = pump_until(&mut pty, &rx, 24, 80, "termie-input-OK", Duration::from_secs(15));
        pty.kill();
        assert!(grid.contains("termie-input-OK"), "typed input not rendered; grid: {grid:?}");
    }

    #[test]
    #[ignore = "spawns a real shell; run locally with `cargo test -- --ignored`"]
    fn pty_keeps_working_after_resize() {
        let mut pty =
            Pty::spawn(24, 80, ShellKind::Cmd, false, None, None, None, "termie", 0, 0)
                .expect("spawn pty");
        let rx = reader_channel(&mut pty);
        std::thread::sleep(Duration::from_millis(500));
        pty.resize(40, 120, 0, 0);
        pty.write(b"echo termie-resize-OK\r\n");
        let grid = pump_until(&mut pty, &rx, 40, 120, "termie-resize-OK", Duration::from_secs(15));
        pty.kill();
        assert!(grid.contains("termie-resize-OK"), "no output rendered after resize; grid: {grid:?}");
    }
}
