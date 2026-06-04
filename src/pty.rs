use std::env;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::thread;

use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

// a prompt hook that emits OSC-7 (cwd) before the prompt text; single-quoted
// pwsh + string concatenation so the command line carries no double quotes
const PWSH_OSC7_PROMPT: &str = r#"function prompt { $p=$PWD.ProviderPath; [char]27+']7;file:///'+($p -replace '\\','/')+[char]27+'\PS '+$p+'> ' }"#;

/// which shell a new pane should launch
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShellKind {
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
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    // reader is parked until start_reader() spawns the output thread; this lets
    // a pane be built off the main thread and only start emitting once registered
    reader: Option<Box<dyn std::io::Read + Send>>,
}

impl Pty {
    /// create the pty + child process (the slow part — safe to call off-thread).
    /// the output thread isn't started until start_reader().
    pub fn spawn(rows: u16, cols: u16, shell: ShellKind, load_profile: bool, cwd: Option<&str>) -> Result<Pty> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let shell = resolve_shell_cached(shell);
        let mut cmd = CommandBuilder::new(&shell);
        // suppress the banner (and the profile unless asked) for a fast prompt, and
        // inject an OSC-7 prompt hook so termie learns the cwd for tab labels / title
        let lower = shell.to_ascii_lowercase();
        if lower.ends_with("pwsh.exe") || lower.ends_with("powershell.exe") {
            cmd.arg("-NoLogo");
            if !load_profile {
                cmd.arg("-NoProfile");
            }
            cmd.arg("-NoExit");
            cmd.arg("-Command");
            cmd.arg(PWSH_OSC7_PROMPT);
        }
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
        // some TUIs only enable the kitty keyboard protocol
        // (the thing that makes shift+enter a newline) when TERM_PROGRAM names a
        // terminal on their static allowlist; they never query the terminal.
        // ghostty is the narrowest-capability match and shares the kitty
        // keyboard family termie genuinely implements
        cmd.env("TERM_PROGRAM", "ghostty");
        // trim pwsh/.NET startup work: skip the background update check + telemetry
        cmd.env("POWERSHELL_UPDATECHECK", "Off");
        cmd.env("POWERSHELL_TELEMETRY_OPTOUT", "1");
        cmd.env("DOTNET_CLI_TELEMETRY_OPTOUT", "1");
        cmd.env("DOTNET_NOLOGO", "1");
        if lower.ends_with("wsl.exe") {
            // forward the terminal env into the WSL distro so colors and the
            // kitty-keyboard hint reach programs running inside wsl
            cmd.env("WSLENV", "TERM/u:COLORTERM/u:TERM_PROGRAM/u");
        }

        let child = pair.slave.spawn_command(cmd)?;
        // close the slave side in the parent so EOF propagates on child exit
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        Ok(Pty {
            master: pair.master,
            writer,
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
            let mut buf = [0u8; 8192];
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

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
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
        Pty {
            master: Box::new(null_pty::NullMaster),
            writer: Box::new(std::io::sink()),
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
