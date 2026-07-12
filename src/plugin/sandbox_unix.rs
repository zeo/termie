//! bubblewrap launcher for plugin subprocesses on linux.
//!
//! spawns a plugin inside a bwrap namespace jail: the filesystem is a read-only
//! view of the system libraries plus the plugin's own install dir, everything
//! else (home, other processes, the session bus) is invisible, and every
//! namespace is unshared — real privilege isolation on top of the subprocess
//! crash isolation the normal host already gives. opt-in via the config
//! `plugin_sandbox=bwrap` (the `on`/`true` spellings work too); the normal
//! spawn path is used otherwise, and a missing bwrap surfaces as a launch error.
//!
//! network access is granted only when the plugin holds the `network`
//! permission (`--share-net`, plus resolv.conf and the CA store so tls works).

use std::io;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// a plugin process running inside a bwrap jail plus the host ends of its
/// stdio pipes. dropping or `kill`ing it stops the process
pub struct Sandboxed {
    child: Child,
}

impl Sandboxed {
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// spawn `program args...` inside a bwrap jail, with `dir` as the working
/// directory and the only writable-free mount the plugin can read, allowing
/// outbound network when `net` is set. `_moniker` names the container on
/// windows; bwrap jails are anonymous
pub fn spawn(
    _moniker: &str,
    program: &Path,
    args: &[String],
    dir: &Path,
    net: bool,
) -> io::Result<Sandboxed> {
    const PLUGIN_DIR: &str = "/plugin";
    let sandbox_program = program
        .strip_prefix(dir)
        .map(|relative| Path::new(PLUGIN_DIR).join(relative))
        .unwrap_or_else(|_| program.to_path_buf());
    let mut cmd = Command::new("bwrap");
    cmd.args(["--die-with-parent", "--unshare-all", "--new-session", "--clearenv"])
        .args(["--setenv", "PATH", "/usr/bin:/bin"])
        .args(["--setenv", "LANG", "C.UTF-8"])
        .args(["--setenv", "TMPDIR", "/tmp"])
        // system libraries + interpreters, read-only; -try tolerates distros
        // where a dir doesn't exist (merged-usr vs split lib layouts)
        .args(["--ro-bind", "/usr", "/usr"])
        .args(["--ro-bind-try", "/lib", "/lib"])
        .args(["--ro-bind-try", "/lib64", "/lib64"])
        .args(["--ro-bind-try", "/bin", "/bin"])
        .args(["--ro-bind-try", "/sbin", "/sbin"])
        .args(["--ro-bind-try", "/etc/ld.so.cache", "/etc/ld.so.cache"])
        .args(["--proc", "/proc"])
        .args(["--dev", "/dev"])
        .args(["--tmpfs", "/tmp"]);
    // use a fixed in-jail path because the host path's parents are intentionally hidden
    cmd.arg("--ro-bind").arg(dir).arg(PLUGIN_DIR);
    if net {
        cmd.args(["--share-net"])
            .args(["--ro-bind-try", "/etc/resolv.conf", "/etc/resolv.conf"])
            .args(["--ro-bind-try", "/etc/ssl", "/etc/ssl"])
            .args(["--ro-bind-try", "/etc/ca-certificates", "/etc/ca-certificates"]);
    }
    cmd.arg("--chdir").arg(PLUGIN_DIR);
    cmd.arg("--").arg(sandbox_program).args(args);
    // discard the plugin's stderr in the sandbox so its logs can't corrupt
    // the protocol stream
    let child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(Sandboxed { child })
}

/// a conservative moniker derived from a plugin id, mirroring the windows
/// naming so logs and config read the same on both platforms
pub fn moniker_for(id: &str) -> String {
    let mut m = format!("termie.plugin.{id}");
    if m.len() > 64 {
        m.truncate(64);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moniker_is_bounded_and_prefixed() {
        assert_eq!(moniker_for("pet"), "termie.plugin.pet");
        assert!(moniker_for(&"x".repeat(100)).len() <= 64);
    }

    // a real end-to-end launch: the jailed child must not see the user's home.
    // #[ignore]d because it needs bwrap installed; run with `cargo test -- --ignored`
    #[test]
    #[ignore = "needs bubblewrap installed"]
    fn sandboxed_child_cannot_see_home() {
        use std::io::Read;
        let dir = std::env::temp_dir();
        let sh = Path::new("/bin/sh");
        let args = vec![
            "-c".to_string(),
            "if [ -e /home ]; then printf visible; else printf hidden; fi".to_string(),
        ];
        let mut sb = spawn("termie.plugin.selftest", sh, &args, &dir, false).expect("bwrap spawn");
        let mut out = String::new();
        sb.take_stdout().unwrap().read_to_string(&mut out).unwrap();
        sb.kill();
        assert_eq!(out, "hidden", "home should be invisible, got: {out:?}");
    }

    #[test]
    #[ignore = "needs bubblewrap installed"]
    fn sandboxed_child_cannot_read_host_environment() {
        use std::io::Read;
        const SECRET: &str = "TERMIE_SANDBOX_TEST_SECRET";
        unsafe { std::env::set_var(SECRET, "visible-on-host") };
        let dir = std::env::temp_dir();
        let sh = Path::new("/bin/sh");
        let args = vec!["-c".to_string(), format!("printf %s \"${{{SECRET}-}}\"")];
        let mut sb = spawn("termie.plugin.env-test", sh, &args, &dir, false).expect("bwrap spawn");
        let mut out = String::new();
        sb.take_stdout().unwrap().read_to_string(&mut out).unwrap();
        sb.kill();
        unsafe { std::env::remove_var(SECRET) };
        assert!(out.is_empty(), "sandbox inherited host secret: {out:?}");
    }
}
