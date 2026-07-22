use std::env;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

const PTY_WRITE_CHUNK_BYTES: usize = 64 * 1024;
const MAX_PTY_WRITE_QUEUE_BYTES: usize = 4 * 1024 * 1024;
const MAX_PTY_WRITE_QUEUE_ITEMS: usize = 64;

struct InputQueue {
    state: Mutex<InputQueueState>,
    ready: Condvar,
}

struct InputQueueState {
    pending: std::collections::VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
}

impl InputQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(InputQueueState {
                pending: std::collections::VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn try_push(&self, bytes: &[u8]) -> bool {
        if bytes.len() > MAX_PTY_WRITE_QUEUE_BYTES {
            return false;
        }
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed
            || state.pending.len() == MAX_PTY_WRITE_QUEUE_ITEMS
            || state.bytes > MAX_PTY_WRITE_QUEUE_BYTES - bytes.len()
        {
            return false;
        }
        state.bytes += bytes.len();
        state.pending.push_back(bytes.to_vec());
        self.ready.notify_one();
        true
    }

    fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(bytes) = state.pending.pop_front() {
                state.bytes -= bytes.len();
                return Some(bytes);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = true;
        self.ready.notify_one();
    }
}

fn queue_write(queue: &InputQueue, bytes: &[u8]) -> bool {
    queue.try_push(bytes)
}

#[cfg(target_os = "linux")]
fn systemd_scope_tools() -> Option<&'static (PathBuf, PathBuf)> {
    static TOOLS: std::sync::OnceLock<Option<(PathBuf, PathBuf)>> = std::sync::OnceLock::new();
    TOOLS
        .get_or_init(|| {
            if !std::path::Path::new("/run/systemd/system").is_dir() {
                return None;
            }
            Some((find_in_path("systemd-run")?, find_in_path("timeout")?))
        })
        .as_ref()
}

#[cfg(target_os = "linux")]
const LINUX_SCOPE_LAUNCHER: &str = r#"timeout=$1; runner=$2; probe=$3; unit=$4; shift 4
if "$timeout" 2s "$runner" --user --scope --quiet --collect --unit "$probe" -- /usr/bin/true >/dev/null 2>&1; then
    exec "$runner" --user --scope --quiet --collect --unit "$unit" -- "$@"
fi
exec "$@""#;

#[cfg(target_os = "linux")]
fn isolate_in_user_scope(cmd: &mut CommandBuilder) {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);
    let Some((systemd_run, timeout)) = systemd_scope_tools() else {
        return;
    };
    let id = NEXT_SCOPE.fetch_add(1, Ordering::Relaxed);
    let unit = format!("termie-pane-{}-{id}", std::process::id());
    let probe = format!("termie-pane-probe-{}-{id}", std::process::id());
    let command = std::mem::take(cmd.get_argv_mut());
    cmd.get_argv_mut().extend([
        "/bin/sh".into(),
        "-c".into(),
        LINUX_SCOPE_LAUNCHER.into(),
        "termie-scope".into(),
        timeout.as_os_str().to_owned(),
        systemd_run.as_os_str().to_owned(),
        probe.into(),
        unit.into(),
    ]);
    cmd.get_argv_mut().extend(command);
}

// a prompt hook that emits OSC-133 command lifecycle marks and OSC-7 cwd
// reporting around the prompt text. it wraps whatever prompt is
// already defined — the pwsh default, or the profile's starship/oh-my-posh
// when load_profile is on — instead of replacing it. single-quoted pwsh +
// string concatenation so the command line carries no double quotes
#[cfg(windows)]
const PWSH_PROMPT_HOOK: &str = r#"$global:__termie_prompt = $function:prompt; function prompt { $ok=$?; $code=if($ok){0}elseif($global:LASTEXITCODE){$global:LASTEXITCODE}else{1}; $p=$PWD.ProviderPath; [char]27+']133;D;'+$code+[char]27+'\'+[char]27+']133;A'+[char]27+'\'+[char]27+']7;file:///'+($p -replace '\\','/')+[char]27+'\'+(& $global:__termie_prompt)+[char]27+']133;B'+[char]27+'\' }"#;

// cmd expands $e to ESC and %PROMPT% before it installs the new prompt
const CMD_PROMPT_HOOK: &str = r#"$e]133;D$e\$e]133;A$e\$e]9;9;$P$e\%PROMPT%$e]133;B$e\"#;

/// the pwsh hook, with the OSC-7 uri shaped per OS: windows paths need the
/// authority-less `file:///C:/...` form, unix paths are already absolute
fn pwsh_prompt_hook() -> &'static str {
    #[cfg(windows)]
    {
        PWSH_PROMPT_HOOK
    }
    #[cfg(not(windows))]
    {
        PWSH_PROMPT_HOOK_UNIX
    }
}

#[cfg(not(windows))]
const PWSH_PROMPT_HOOK_UNIX: &str = r#"$global:__termie_prompt = $function:prompt; function prompt { $ok=$?; $code=if($ok){0}elseif($global:LASTEXITCODE){$global:LASTEXITCODE}else{1}; $p=$PWD.ProviderPath; [char]27+']133;D;'+$code+[char]27+'\'+[char]27+']133;A'+[char]27+'\'+[char]27+']7;file://'+$p+[char]27+'\'+(& $global:__termie_prompt)+[char]27+']133;B'+[char]27+'\' }"#;

// fish registers the hooks inline via -C; no rc file dance needed
#[cfg(unix)]
const FISH_PROMPT_HOOK: &str = r#"function __termie_preexec --on-event fish_preexec; printf '\033]133;B\033\\'; printf '\033]133;C\033\\'; end; function __termie_postexec --on-event fish_postexec; printf '\033]133;D;%d\033\\' $status; end; function __termie_prompt --on-event fish_prompt; printf '\033]133;A\033\\'; printf '\033]7;file://%s\033\\' $PWD; end"#;

#[cfg(unix)]
const BASH_RC: &str = r#"# termie's bash integration: source the user's own rc, then add lifecycle marks
[ -r "$HOME/.bashrc" ] && . "$HOME/.bashrc"
__termie_prompt() { local s=$?; printf '\033]133;D;%d\033\\\033]133;A\033\\\033]7;file://%s\033\\' "$s" "$PWD"; }
# bel avoids escaping a leading command substitution in an existing PS0
PS0=$'\033]133;B\007\033]133;C\007'"${PS0-}"
PROMPT_COMMAND="__termie_prompt${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
"#;

#[cfg(unix)]
const ZSH_ENV: &str = r#"# termie's zsh integration: restore the user's startup directory, then add hooks
ZDOTDIR="${TERMIE_USER_ZDOTDIR:-$HOME}"
unset TERMIE_USER_ZDOTDIR
[ -r "$ZDOTDIR/.zshenv" ] && . "$ZDOTDIR/.zshenv"
autoload -Uz add-zsh-hook
__termie_preexec() { printf '\033]133;B\033\\\033]133;C\033\\'; }
__termie_precmd() { local s=$?; printf '\033]133;D;%d\033\\\033]133;A\033\\\033]7;file://%s\033\\' "$s" "$PWD"; }
add-zsh-hook preexec __termie_preexec
add-zsh-hook precmd __termie_precmd
"#;

/// write the bash/zsh integration files once per process and hand back their
/// dir; None (and a plain shell, no hook) when the config dir is unavailable
#[cfg(unix)]
fn integration_dir() -> Option<std::path::PathBuf> {
    static DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = crate::cache_dir()?.join("shell");
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(dir.join("bashrc"), BASH_RC).ok()?;
        std::fs::write(dir.join(".zshenv"), ZSH_ENV).ok()?;
        Some(dir)
    })
    .clone()
}

// WSLENV names which windows env vars cross into the linux side, so colors and
// terminal identity reach programs running inside a distro
const WSLENV_FORWARD: &str = "TERM/u:COLORTERM/u:TERM_PROGRAM/u:TERM_PROGRAM_VERSION/u:TERMIE/u";

/// which shell a new pane should launch. every variant exists on both
/// platforms (a session snapshot written on one OS must load on the other),
/// but cycling, labels-from-config, and resolution only honor the kinds that
/// exist on the running OS — a foreign label degrades to Auto
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ShellKind {
    #[default]
    Auto,
    Pwsh,
    // the windows shells exist in the enum on unix too (cross-OS session
    // snapshots), but nothing on unix ever constructs them — and vice versa
    #[cfg_attr(not(windows), allow(dead_code))]
    PowerShell,
    #[cfg_attr(not(windows), allow(dead_code))]
    Cmd,
    #[cfg_attr(not(windows), allow(dead_code))]
    Wsl,
    #[cfg_attr(windows, allow(dead_code))]
    Bash,
    #[cfg_attr(windows, allow(dead_code))]
    Zsh,
    #[cfg_attr(windows, allow(dead_code))]
    Fish,
    /// a config-defined profile (`profile.<name>=<command line>`); the name
    /// borrows from the process-lifetime registry, keeping the enum Copy
    Custom(&'static str),
}

impl ShellKind {
    #[cfg(windows)]
    pub fn next(self) -> Self {
        match self {
            ShellKind::Auto => ShellKind::Pwsh,
            ShellKind::Pwsh => ShellKind::PowerShell,
            ShellKind::PowerShell => ShellKind::Cmd,
            ShellKind::Cmd => ShellKind::Wsl,
            _ => ShellKind::Auto,
        }
    }

    #[cfg(not(windows))]
    pub fn next(self) -> Self {
        match self {
            ShellKind::Auto => ShellKind::Bash,
            ShellKind::Bash => ShellKind::Zsh,
            ShellKind::Zsh => ShellKind::Fish,
            ShellKind::Fish => ShellKind::Pwsh,
            _ => ShellKind::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ShellKind::Auto => "auto",
            ShellKind::Pwsh => "pwsh",
            ShellKind::PowerShell => "powershell",
            ShellKind::Cmd => "cmd",
            ShellKind::Wsl => "wsl",
            ShellKind::Bash => "bash",
            ShellKind::Zsh => "zsh",
            ShellKind::Fish => "fish",
            ShellKind::Custom(name) => name,
        }
    }

    pub fn from_label(s: &str) -> Self {
        match s {
            "pwsh" => ShellKind::Pwsh,
            #[cfg(windows)]
            "powershell" => ShellKind::PowerShell,
            #[cfg(windows)]
            "cmd" => ShellKind::Cmd,
            #[cfg(windows)]
            "wsl" => ShellKind::Wsl,
            #[cfg(not(windows))]
            "bash" => ShellKind::Bash,
            #[cfg(not(windows))]
            "zsh" => ShellKind::Zsh,
            #[cfg(not(windows))]
            "fish" => ShellKind::Fish,
            // a session snapshot or config may name a custom profile; unknown
            // names (a profile since removed from config, or a shell that only
            // exists on the other OS) fall back to auto
            other => match profiles().iter().find(|p| p.name == other) {
                Some(p) => ShellKind::Custom(p.name.as_str()),
                None => ShellKind::Auto,
            },
        }
    }
}

/// a config-defined custom shell profile: `profile.<name>=<argv>`, plus the
/// optional `profile.<name>.cwd=<dir>` and `profile.<name>.env.<VAR>=<value>`
/// sub-keys
#[derive(Clone)]
pub struct Profile {
    pub name: String,
    pub argv: Vec<String>,
    /// working dir the profile launches in, overriding the caller's cwd
    pub cwd: Option<String>,
    /// extra environment variables layered on top of the spawn defaults
    pub env: Vec<(String, String)>,
}

/// config-defined custom shell profiles, set once at startup
static PROFILES: std::sync::OnceLock<Vec<Profile>> = std::sync::OnceLock::new();

/// install the custom profiles parsed from config. only the first call takes
/// effect — the registry hands out 'static borrows, so it can never mutate
pub fn set_profiles(profiles: Vec<Profile>) {
    let _ = PROFILES.set(profiles);
}

pub fn profiles() -> &'static [Profile] {
    PROFILES.get().map(Vec::as_slice).unwrap_or(&[])
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
    writer_queue: Arc<InputQueue>,
    child: Box<dyn Child + Send + Sync>,
    // reader is parked until start_reader() spawns the output thread; this lets
    // a pane be built off the main thread and only start emitting once registered
    reader: Option<Box<dyn std::io::Read + Send>>,
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.writer_queue.close();
    }
}

impl Pty {
    /// create the pty + child process (the slow part — safe to call off-thread).
    /// the output thread isn't started until start_reader().
    #[allow(clippy::too_many_arguments)]
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

        // a custom profile carries its own argv (routed through the explicit-
        // command path below so user args never mix with hook injection), plus
        // an optional working dir and extra env applied further down
        let profile = match shell {
            ShellKind::Custom(name) => profiles().iter().find(|p| p.name == name),
            _ => None,
        };
        let command = match (shell, command) {
            (ShellKind::Custom(_), None) => profile.map(|p| p.argv.as_slice()),
            _ => command,
        };
        // a profile's cwd pins its launch dir over whatever the caller passed
        let cwd = profile.and_then(|p| p.cwd.as_deref()).or(cwd);
        // an explicit command (from the cli or a context-menu verb) runs directly
        // instead of a login shell, so none of the shell banner/prompt-hook
        // injection applies; otherwise launch the configured shell
        let mut cmd = match command.filter(|a| !a.is_empty()) {
            Some(argv) => {
                let mut c = CommandBuilder::new(&argv[0]);
                for a in &argv[1..] {
                    c.arg(a.as_str());
                }
                // a custom profile (e.g. a synthetic "WSL: <distro>") that launches
                // wsl needs the same env bridge the built-in wsl shell gets
                if argv[0].to_ascii_lowercase().ends_with("wsl.exe") {
                    c.env("WSLENV", WSLENV_FORWARD);
                }
                c
            }
            None => {
                let shell = resolve_shell_cached(shell);
                // suppress the banner (and the profile unless asked) for a fast
                // prompt, and inject an OSC-7 hook so termie learns the cwd
                let stem = std::path::Path::new(&shell)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                let mut c = CommandBuilder::new(&shell);
                match stem.as_str() {
                    "pwsh" | "powershell" => {
                        c.arg("-NoLogo");
                        if !load_profile {
                            c.arg("-NoProfile");
                        }
                        c.arg("-NoExit");
                        c.arg("-Command");
                        c.arg(pwsh_prompt_hook());
                    }
                    "cmd" => {
                        c.arg("/K");
                        c.arg(format!("prompt {CMD_PROMPT_HOOK}"));
                    }
                    "wsl" => {
                        // launch a specific distro when one is configured (else the
                        // wsl default), and forward the terminal env in so colors and
                        // identity reach programs running inside wsl
                        if let Some(d) = wsl_distro {
                            c.arg("-d");
                            c.arg(d);
                        }
                        c.env("WSLENV", WSLENV_FORWARD);
                    }
                    #[cfg(unix)]
                    "bash" => {
                        // an rcfile that sources the user's own ~/.bashrc first,
                        // so termie's prompt hook wraps it instead of replacing it
                        if let Some(dir) = integration_dir() {
                            c.arg("--rcfile");
                            c.arg(dir.join("bashrc"));
                        }
                        c.arg("-i");
                    }
                    #[cfg(unix)]
                    "zsh" => {
                        if let Some(dir) = integration_dir() {
                            c.env(
                                "TERMIE_USER_ZDOTDIR",
                                env::var_os("ZDOTDIR").or_else(|| env::var_os("HOME")).unwrap_or_default(),
                            );
                            c.env("ZDOTDIR", dir);
                        }
                        c.arg("-i");
                    }
                    #[cfg(unix)]
                    "fish" => {
                        c.arg("-i");
                        c.arg("-C");
                        c.arg(FISH_PROMPT_HOOK);
                    }
                    _ => {}
                }
                c
            }
        };
        // start in the requested directory (a new tab/split in the focused repo),
        // falling back to home if it's unset or no longer a valid directory
        if let Some(dir) = cwd.filter(|d| std::path::Path::new(*d).is_dir()) {
            cmd.cwd(dir);
        } else if let Some(home) = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
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
        // a profile's env vars go on last so it can override any default above
        if let Some(p) = profile {
            for (k, val) in &p.env {
                cmd.env(k, val);
            }
        }

        // separate panes so pressure kills one workload instead of the whole terminal
        #[cfg(target_os = "linux")]
        isolate_in_user_scope(&mut cmd);

        let child = pair.slave.spawn_command(cmd)?;
        // close the slave side in the parent so EOF propagates on child exit
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let writer_queue = Arc::new(InputQueue::new());
        let writer_work = Arc::clone(&writer_queue);
        thread::spawn(move || {
            // exits when the pty drops or the pipe breaks
            while let Some(bytes) = writer_work.pop() {
                let wrote = bytes.chunks(PTY_WRITE_CHUNK_BYTES).all(|chunk| writer.write_all(chunk).is_ok());
                if !wrote || writer.flush().is_err() {
                    break;
                }
            }
        });

        Ok(Pty {
            master: pair.master,
            writer_queue,
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
    pub fn write(&mut self, bytes: &[u8]) -> bool {
        queue_write(&self.writer_queue, bytes)
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
#[cfg(windows)]
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
        // only reached when a profile vanished from config or has an empty
        // command line; fall back like Auto does — and for the shells that only
        // exist on unix, which a cross-OS session snapshot can still name
        _ => auto(),
    }
}

/// resolve a shell kind to an executable path. Auto prefers the user's login
/// shell ($SHELL), then falls back through the common shells
#[cfg(not(windows))]
fn resolve_shell(kind: ShellKind) -> String {
    let auto = || {
        env::var("SHELL")
            .ok()
            .filter(|s| std::path::Path::new(s).is_file())
            .or_else(|| find_in_path("bash").map(|p| p.to_string_lossy().into_owned()))
            .or_else(|| find_in_path("zsh").map(|p| p.to_string_lossy().into_owned()))
            .or_else(|| find_in_path("fish").map(|p| p.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "/bin/sh".to_string())
    };
    let find = |exe: &str| find_in_path(exe).map(|p| p.to_string_lossy().into_owned());
    match kind {
        ShellKind::Auto => auto(),
        ShellKind::Bash => find("bash").unwrap_or_else(auto),
        ShellKind::Zsh => find("zsh").unwrap_or_else(auto),
        ShellKind::Fish => find("fish").unwrap_or_else(auto),
        ShellKind::Pwsh => find("pwsh").unwrap_or_else(auto),
        // windows-only kinds (a cross-OS session snapshot) and vanished
        // profiles fall back like Auto does
        _ => auto(),
    }
}

#[cfg(windows)]
impl Pty {
    /// wrap a default-terminal handoff session (raw ConPTY pipes received over
    /// COM) as a Pty, so the pane machinery treats it like any spawned shell.
    /// resizes go down the ConPTY signal pipe; killing the pane terminates the
    /// client. the reference/server handles ride along so the console session
    /// stays alive exactly as long as this Pty
    pub fn from_handoff(
        reader: std::os::windows::io::OwnedHandle,
        writer: std::os::windows::io::OwnedHandle,
        signal: std::os::windows::io::OwnedHandle,
        reference: std::os::windows::io::OwnedHandle,
        server: std::os::windows::io::OwnedHandle,
        client: std::os::windows::io::OwnedHandle,
    ) -> Pty {
        use std::fs::File;
        let mut writer = File::from(writer);
        let writer_queue = Arc::new(InputQueue::new());
        let writer_work = Arc::clone(&writer_queue);
        thread::spawn(move || {
            while let Some(bytes) = writer_work.pop() {
                let wrote = bytes.chunks(PTY_WRITE_CHUNK_BYTES).all(|chunk| writer.write_all(chunk).is_ok());
                if !wrote || writer.flush().is_err() {
                    break;
                }
            }
        });
        Pty {
            master: Box::new(handoff_pty::HandoffMaster::new(signal, reference, server)),
            writer_queue,
            child: Box::new(handoff_pty::HandoffChild::new(client)),
            reader: Some(Box::new(File::from(reader))),
        }
    }
}

// pty backend for default-terminal handoff sessions: no process is spawned
// here — the ConPTY already exists in the console host, reachable only through
// the handles COM delivered
#[cfg(windows)]
mod handoff_pty {
    use super::*;
    use portable_pty::{ChildKiller, ExitStatus};
    use std::fs::File;
    use std::io::Result as IoResult;
    use std::os::windows::io::{AsRawHandle, OwnedHandle};
    use std::sync::{Arc, Mutex};

    /// ConPTY out-of-band resize packet id (winconpty.h PTY_SIGNAL_RESIZE_WINDOW)
    const SIGNAL_RESIZE: u16 = 8;

    pub struct HandoffMaster {
        signal: Mutex<File>,
        size: Mutex<PtySize>,
        /// console driver reference + console host process: held for the
        /// session's lifetime, released when the pane closes
        _reference: OwnedHandle,
        _server: OwnedHandle,
    }

    impl HandoffMaster {
        pub fn new(signal: OwnedHandle, reference: OwnedHandle, server: OwnedHandle) -> Self {
            HandoffMaster {
                signal: Mutex::new(File::from(signal)),
                size: Mutex::new(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }),
                _reference: reference,
                _server: server,
            }
        }
    }

    impl MasterPty for HandoffMaster {
        fn resize(&self, size: PtySize) -> anyhow::Result<()> {
            let packet = [SIGNAL_RESIZE, size.cols, size.rows];
            let mut f = self.signal.lock().unwrap();
            f.write_all(bytemuck::cast_slice(&packet))?;
            f.flush()?;
            *self.size.lock().unwrap() = size;
            Ok(())
        }
        fn get_size(&self) -> anyhow::Result<PtySize> {
            Ok(*self.size.lock().unwrap())
        }
        // reader/writer were taken directly from the handoff pipes when the
        // Pty was built; nothing should come back for a second copy
        fn try_clone_reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
            anyhow::bail!("handoff pty reader is owned by the pane")
        }
        fn take_writer(&self) -> anyhow::Result<Box<dyn std::io::Write + Send>> {
            anyhow::bail!("handoff pty writer is owned by the pane")
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

    /// the already-running client application, addressed by process handle
    #[derive(Debug)]
    pub struct HandoffChild {
        client: Arc<OwnedHandle>,
    }

    impl HandoffChild {
        pub fn new(client: OwnedHandle) -> Self {
            HandoffChild { client: Arc::new(client) }
        }
        fn handle(&self) -> windows::Win32::Foundation::HANDLE {
            windows::Win32::Foundation::HANDLE(self.client.as_raw_handle())
        }
        fn exit_code(&self) -> Option<u32> {
            use windows::Win32::Foundation::WAIT_OBJECT_0;
            use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
            unsafe {
                if WaitForSingleObject(self.handle(), 0) != WAIT_OBJECT_0 {
                    return None;
                }
                let mut code = 0u32;
                GetExitCodeProcess(self.handle(), &mut code).ok().map(|_| code)
            }
        }
    }

    impl ChildKiller for HandoffChild {
        fn kill(&mut self) -> IoResult<()> {
            use windows::Win32::System::Threading::TerminateProcess;
            unsafe {
                let _ = TerminateProcess(self.handle(), 1);
            }
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(HandoffChild { client: self.client.clone() })
        }
    }

    impl Child for HandoffChild {
        fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
            Ok(self.exit_code().map(ExitStatus::with_exit_code))
        }
        fn wait(&mut self) -> IoResult<ExitStatus> {
            use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};
            unsafe {
                WaitForSingleObject(self.handle(), INFINITE);
            }
            Ok(ExitStatus::with_exit_code(self.exit_code().unwrap_or(0)))
        }
        fn process_id(&self) -> Option<u32> {
            use windows::Win32::System::Threading::GetProcessId;
            match unsafe { GetProcessId(self.handle()) } {
                0 => None,
                pid => Some(pid),
            }
        }
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            Some(self.client.as_raw_handle())
        }
    }
}

#[cfg(test)]
impl Pty {
    /// a no-op pty for tests: build a Pane without spawning a real shell
    pub(crate) fn null() -> Pty {
        let writer_queue = Arc::new(InputQueue::new());
        Pty {
            master: Box::new(null_pty::NullMaster),
            writer_queue,
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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_queue_rejects_a_full_input_without_queuing_a_prefix() {
        let queue = InputQueue::new();
        let input = vec![b'x'; MAX_PTY_WRITE_QUEUE_BYTES];
        assert!(queue_write(&queue, &input));
        assert!(!queue_write(&queue, b"y"));
        assert_eq!(queue.pop(), Some(input));
        queue.close();
        assert_eq!(queue.pop(), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_scope_probe_falls_back_to_direct_command() {
        let output = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                LINUX_SCOPE_LAUNCHER,
                "termie-scope",
                "/usr/bin/timeout",
                "/usr/bin/false",
                "probe",
                "pane",
                "/usr/bin/printf",
                "shell-ok",
            ])
            .output()
            .expect("run scope fallback");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"shell-ok");
    }

    // one test owns every profile-registry assertion: the OnceLock takes only
    // the first set_profiles of the process, so splitting these across tests
    // would make them order-dependent
    #[test]
    fn custom_profiles_round_trip_labels() {
        set_profiles(vec![
            Profile {
                name: "git-bash".to_string(),
                argv: vec!["C:\\Git\\bin\\bash.exe".to_string(), "-i".to_string()],
                cwd: None,
                env: Vec::new(),
            },
            Profile { name: "nu".to_string(), argv: vec!["nu.exe".to_string()], cwd: None, env: Vec::new() },
        ]);
        assert_eq!(profiles().len(), 2);
        let k = ShellKind::from_label("git-bash");
        assert!(matches!(k, ShellKind::Custom(name) if name == "git-bash"));
        // a session snapshot stores label(); it must come back as the profile
        assert_eq!(ShellKind::from_label(k.label()), k);
        // a profile removed from config degrades to auto, never panics
        assert_eq!(ShellKind::from_label("gone"), ShellKind::Auto);
        // built-ins are never shadowed by the profile lookup, and a label
        // that only exists on the other OS degrades to auto
        #[cfg(windows)]
        {
            assert_eq!(ShellKind::from_label("cmd"), ShellKind::Cmd);
            assert_eq!(ShellKind::from_label("bash"), ShellKind::Auto);
        }
        #[cfg(not(windows))]
        {
            assert_eq!(ShellKind::from_label("bash"), ShellKind::Bash);
            assert_eq!(ShellKind::from_label("cmd"), ShellKind::Auto);
        }
        // cycling out of a custom profile lands on auto
        assert_eq!(k.next(), ShellKind::Auto);
    }
}

// live integration tests, unix flavor: the same spawn -> read -> parse path
// through a real /bin/sh. #[ignore]d like the windows set; run locally with
// `cargo test -- --ignored`
#[cfg(all(test, unix))]
mod live_tests_unix {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "spawns a real shell; run locally with `cargo test -- --ignored`"]
    fn pty_runs_a_command_and_streams_output() {
        let argv = ["/bin/sh", "-c", "echo termie-itest-OK"].map(String::from);
        let mut pty =
            Pty::spawn(24, 80, ShellKind::Auto, false, None, Some(&argv[..]), None, "termie", 0, 0)
                .expect("spawn pty");
        let (tx, rx) = mpsc::channel();
        pty.start_reader(move |m| {
            let _ = tx.send(m);
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut out = Vec::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(PtyMsg::Output(b)) => {
                    out.extend_from_slice(&b);
                    if out.windows(b"termie-itest-OK".len()).any(|w| w == b"termie-itest-OK") {
                        break;
                    }
                }
                Ok(PtyMsg::Exited) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        pty.kill();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("termie-itest-OK"), "output not seen; got: {text:?}");
    }

    fn assert_shell_lifecycle(shell: ShellKind, executable: &str) {
        if find_in_path(executable).is_none() {
            eprintln!("skip: no {executable} on PATH");
            return;
        }
        let mut pty =
            Pty::spawn(24, 80, shell, false, None, None, None, "termie", 0, 0)
                .expect("spawn pty");
        let (tx, rx) = mpsc::channel();
        pty.start_reader(move |m| {
            let _ = tx.send(m);
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut out = Vec::new();
        let needle = b"\x1b]133;A\x1b\\";
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(PtyMsg::Output(b)) => {
                    out.extend_from_slice(&b);
                    if out.windows(needle.len()).any(|w| w == needle) {
                        break;
                    }
                }
                Ok(PtyMsg::Exited) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        pty.write(b"false\r");
        let done = b"\x1b]133;D;1\x1b\\";
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(PtyMsg::Output(b)) => {
                    out.extend_from_slice(&b);
                    if out.windows(done.len()).any(|w| w == done) {
                        break;
                    }
                }
                Ok(PtyMsg::Exited) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        pty.kill();
        assert!(
            out.windows(needle.len()).any(|w| w == needle),
            "{executable} never emitted a prompt mark: {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            out.windows(b"\x1b]7;file://".len()).any(|w| w == b"\x1b]7;file://"),
            "no OSC 7 cwd report"
        );
        let command_marks: [&[u8]; 2] = if shell == ShellKind::Bash {
            [b"\x1b]133;B\x07", b"\x1b]133;C\x07"]
        } else {
            [b"\x1b]133;B\x1b\\", b"\x1b]133;C\x1b\\"]
        };
        for mark in command_marks.into_iter().chain([done.as_slice()]) {
            assert!(
                out.windows(mark.len()).any(|w| w == mark),
                "missing lifecycle mark {mark:?}: {:?}",
                String::from_utf8_lossy(&out)
            );
        }
    }

    #[test]
    #[ignore = "spawns a real shell; run locally with `cargo test -- --ignored`"]
    fn bash_integration_hook_emits_lifecycle_marks() {
        assert_shell_lifecycle(ShellKind::Bash, "bash");
    }

    #[test]
    #[ignore = "spawns a real shell; run locally with `cargo test -- --ignored`"]
    fn zsh_integration_hook_emits_lifecycle_marks() {
        assert_shell_lifecycle(ShellKind::Zsh, "zsh");
    }

    #[test]
    #[ignore = "spawns a real shell; run locally with `cargo test -- --ignored`"]
    fn fish_integration_hook_emits_lifecycle_marks() {
        assert_shell_lifecycle(ShellKind::Fish, "fish");
    }
}

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

    #[test]
    #[ignore = "spawns a real shell; run locally with `cargo test -- --ignored`"]
    fn cmd_prompt_emits_shell_integration() {
        let mut pty =
            Pty::spawn(24, 80, ShellKind::Cmd, false, None, None, None, "termie", 0, 0)
                .expect("spawn pty");
        let rx = reader_channel(&mut pty);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        let mut term = Terminal::new(24, 80);
        let mut parser = Parser::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(PtyMsg::Output(bytes)) => {
                    output.extend_from_slice(&bytes);
                    parser.advance(&mut term, &bytes);
                    if !term.responses.is_empty() {
                        pty.write(&std::mem::take(&mut term.responses));
                    }
                    if output.windows(b"\x1b]133;A\x1b\\".len()).any(|s| s == b"\x1b]133;A\x1b\\") {
                        break;
                    }
                }
                Ok(PtyMsg::Exited) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        pty.kill();
        assert!(
            output.windows(b"\x1b]133;A\x1b\\".len()).any(|s| s == b"\x1b]133;A\x1b\\"),
            "cmd never emitted a prompt mark: {output:?}"
        );
        assert!(output.windows(b"\x1b]9;9;".len()).any(|s| s == b"\x1b]9;9;"));
    }
}
