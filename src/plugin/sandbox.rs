//! windows appcontainer launcher for plugin subprocesses.
//!
//! spawns a plugin inside an appcontainer so it runs at low integrity with no
//! access to the user's files, registry, network, windows, or other processes
//! unless a capability is granted — real privilege isolation on top of the
//! subprocess crash isolation the normal host already gives. opt-in via the
//! config `plugin_sandbox=appcontainer`; the normal spawn path is used otherwise.
//!
//! network access is granted only when the plugin holds the `network` permission
//! (the internetClient capability). the plugin's own install directory is granted
//! read+execute to ALL APPLICATION PACKAGES so the appcontainer can load its exe
//! and assets — the same ace a packaged app's install dir carries.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use windows::core::{Error as WinError, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, SetHandleInformation, ERROR_ALREADY_EXISTS, HANDLE, HANDLE_FLAGS,
    HANDLE_FLAG_INHERIT, HLOCAL,
};
use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows::Win32::Security::{
    FreeSid, PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    TerminateProcess, UpdateProcThreadAttribute, CREATE_NO_WINDOW, EXTENDED_STARTUPINFO_PRESENT,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

/// the well-known capability sid for outbound network (internetClient)
const INTERNET_CLIENT_SID: &str = "S-1-15-3-1";
/// the well-known sid for ALL APPLICATION PACKAGES
const ALL_APP_PACKAGES_SID: &str = "*S-1-15-2-1";
/// SE_GROUP_ENABLED: mark a capability sid as enabled in the token
const SE_GROUP_ENABLED: u32 = 0x0000_0004;

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn to_io(e: WinError) -> io::Error {
    io::Error::other(e.message())
}

/// a plugin process running inside an appcontainer plus the host ends of its
/// stdio pipes. dropping or `kill`ing it stops the process
pub struct Sandboxed {
    process: HANDLE,
    stdin: Option<File>,
    stdout: Option<File>,
}

// the process handle is an owned os handle; moving it to the host's plugin list
// (and dropping it there) is safe
unsafe impl Send for Sandboxed {}

impl Sandboxed {
    pub fn take_stdin(&mut self) -> Option<File> {
        self.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<File> {
        self.stdout.take()
    }

    pub fn kill(&mut self) {
        if !self.process.is_invalid() {
            unsafe {
                let _ = TerminateProcess(self.process, 1);
                let _ = CloseHandle(self.process);
            }
            self.process = HANDLE::default();
        }
    }
}

impl Drop for Sandboxed {
    fn drop(&mut self) {
        if !self.process.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.process);
            }
        }
    }
}

/// spawn `program args...` inside an appcontainer named `moniker`, with `dir` as
/// the working directory and as the directory granted to the container, allowing
/// outbound network when `net` is set
pub fn spawn(
    moniker: &str,
    program: &Path,
    args: &[String],
    dir: &Path,
    net: bool,
) -> io::Result<Sandboxed> {
    spawn_inner(moniker, program, args, dir, net)
}

fn spawn_inner(
    moniker: &str,
    program: &Path,
    args: &[String],
    dir: &Path,
    net: bool,
) -> io::Result<Sandboxed> {
    // grant the container read+execute on its own install dir (exe + assets);
    // this shells out to icacls and has no unsafe surface
    grant_app_packages(dir);

    let app = wide(&program.to_string_lossy());
    let mut cmdline = build_cmdline(program, args);
    let cwd = wide(&dir.to_string_lossy());
    let name = wide(moniker);

    unsafe {
        // 1. appcontainer profile -> package sid (create, or derive if it exists)
        let app_sid: PSID = match CreateAppContainerProfile(
            PCWSTR(name.as_ptr()),
            PCWSTR(name.as_ptr()),
            PCWSTR(name.as_ptr()),
            None,
        ) {
            Ok(s) => s,
            Err(e) if e.code() == ERROR_ALREADY_EXISTS.to_hresult() => {
                DeriveAppContainerSidFromAppContainerName(PCWSTR(name.as_ptr())).map_err(to_io)?
            }
            Err(e) => return Err(to_io(e)),
        };

        // 2. optional internetClient capability
        let mut cap_sid: PSID = PSID::default();
        let mut caps: [SID_AND_ATTRIBUTES; 1] = [SID_AND_ATTRIBUTES::default()];
        let mut sec = SECURITY_CAPABILITIES {
            AppContainerSid: app_sid,
            ..Default::default()
        };
        if net {
            let s = wide(INTERNET_CLIENT_SID);
            if ConvertStringSidToSidW(PCWSTR(s.as_ptr()), &mut cap_sid).is_ok() {
                caps[0] = SID_AND_ATTRIBUTES {
                    Sid: cap_sid,
                    Attributes: SE_GROUP_ENABLED,
                };
                sec.Capabilities = caps.as_mut_ptr();
                sec.CapabilityCount = 1;
            }
        }

        // 3. stdio pipes; the host ends must not be inherited by the child
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: true.into(),
            ..Default::default()
        };
        let (mut child_stdin_r, mut host_stdin_w) = (HANDLE::default(), HANDLE::default());
        let (mut host_stdout_r, mut child_stdout_w) = (HANDLE::default(), HANDLE::default());
        if let Err(e) = CreatePipe(&mut child_stdin_r, &mut host_stdin_w, Some(&sa), 0) {
            free_sids(app_sid, cap_sid);
            return Err(to_io(e));
        }
        if let Err(e) = CreatePipe(&mut host_stdout_r, &mut child_stdout_w, Some(&sa), 0) {
            close_all(&[child_stdin_r, host_stdin_w]);
            free_sids(app_sid, cap_sid);
            return Err(to_io(e));
        }
        let _ = SetHandleInformation(host_stdin_w, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
        let _ = SetHandleInformation(host_stdout_r, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));

        // 4. proc-thread attribute list carrying the security capabilities
        let mut size: usize = 0;
        let _ = InitializeProcThreadAttributeList(None, 1, Some(0), &mut size);
        let mut attr_buf = vec![0u8; size];
        let attr_list = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr() as *mut _);
        if let Err(e) = InitializeProcThreadAttributeList(Some(attr_list), 1, Some(0), &mut size) {
            close_all(&[child_stdin_r, host_stdin_w, host_stdout_r, child_stdout_w]);
            free_sids(app_sid, cap_sid);
            return Err(to_io(e));
        }
        if let Err(e) = UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            Some(&sec as *const _ as *const _),
            size_of::<SECURITY_CAPABILITIES>(),
            None,
            None,
        ) {
            DeleteProcThreadAttributeList(attr_list);
            close_all(&[child_stdin_r, host_stdin_w, host_stdout_r, child_stdout_w]);
            free_sids(app_sid, cap_sid);
            return Err(to_io(e));
        }

        // 5. launch
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attr_list;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = child_stdin_r;
        si.StartupInfo.hStdOutput = child_stdout_w;
        // discard the plugin's stderr in the sandbox so its logs can't corrupt
        // the protocol stream
        si.StartupInfo.hStdError = HANDLE::default();
        let mut pi = PROCESS_INFORMATION::default();
        let res = CreateProcessW(
            PCWSTR(app.as_ptr()),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            true,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW,
            None,
            PCWSTR(cwd.as_ptr()),
            &si as *const STARTUPINFOEXW as *const STARTUPINFOW,
            &mut pi,
        );

        DeleteProcThreadAttributeList(attr_list);
        free_sids(app_sid, cap_sid);
        // the child holds its ends now; the host keeps only its own
        let _ = CloseHandle(child_stdin_r);
        let _ = CloseHandle(child_stdout_w);

        if let Err(e) = res {
            let _ = CloseHandle(host_stdin_w);
            let _ = CloseHandle(host_stdout_r);
            return Err(to_io(e));
        }
        let _ = CloseHandle(pi.hThread);

        let stdin = File::from_raw_handle(host_stdin_w.0 as *mut _);
        let stdout = File::from_raw_handle(host_stdout_r.0 as *mut _);
        Ok(Sandboxed {
            process: pi.hProcess,
            stdin: Some(stdin),
            stdout: Some(stdout),
        })
    }
}

fn free_sids(app_sid: PSID, cap_sid: PSID) {
    unsafe {
        if !app_sid.0.is_null() {
            let _ = FreeSid(app_sid);
        }
        if !cap_sid.0.is_null() {
            // ConvertStringSidToSidW allocates with LocalAlloc
            let _ = LocalFree(Some(HLOCAL(cap_sid.0)));
        }
    }
}

fn close_all(handles: &[HANDLE]) {
    unsafe {
        for &h in handles {
            if !h.is_invalid() {
                let _ = CloseHandle(h);
            }
        }
    }
}

fn push_quoted_arg(line: &mut String, arg: &str) {
    line.push('"');
    let mut backslashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                line.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                line.push('"');
                backslashes = 0;
            }
            _ => {
                line.extend(std::iter::repeat_n('\\', backslashes));
                line.push(ch);
                backslashes = 0;
            }
        }
    }
    line.extend(std::iter::repeat_n('\\', backslashes * 2));
    line.push('"');
}

/// build a command line that the C runtime parses back into the original argv
fn build_cmdline(program: &Path, args: &[String]) -> Vec<u16> {
    let mut s = String::new();
    push_quoted_arg(&mut s, &program.to_string_lossy());
    for a in args {
        s.push(' ');
        push_quoted_arg(&mut s, a);
    }
    wide(&s)
}

/// grant ALL APPLICATION PACKAGES read+execute on `dir` (inheritable) so an
/// appcontainer can load the plugin exe. idempotent; failures are non-fatal and
/// surface later as a launch error
fn grant_app_packages(dir: &Path) {
    let _ = Command::new("icacls")
        .arg(dir)
        .arg("/grant")
        .arg(format!("{ALL_APP_PACKAGES_SID}:(OI)(CI)(RX)"))
        .arg("/T")
        .arg("/C")
        .arg("/Q")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW.0)
        .status();
}

/// a conservative appcontainer moniker derived from a plugin id (already a safe
/// single path segment); bounded to the 64-char appcontainer name limit
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

    #[test]
    fn cmdline_quotes_program_and_args() {
        let line = build_cmdline(
            Path::new("C:\\a b\\p.exe"),
            &["--x".into(), "y z".into(), "quote\"here".into(), "tail\\".into()],
        );
        let s = String::from_utf16(&line[..line.len() - 1]).unwrap();
        assert_eq!(s, "\"C:\\a b\\p.exe\" \"--x\" \"y z\" \"quote\\\"here\" \"tail\\\\\"");
    }

    // a real end-to-end launch: confirms a spawned child's token actually reports
    // appcontainer membership, i.e. the isolation is applied (not just that
    // CreateProcess succeeded). #[ignore]d because it creates an appcontainer
    // profile + process; run on demand with `cargo test -- --ignored`
    #[test]
    #[ignore = "creates an appcontainer profile + process"]
    fn sandboxed_child_runs_in_an_appcontainer() {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Security::Isolation::DeleteAppContainerProfile;
        use windows::Win32::Security::{GetTokenInformation, TokenIsAppContainer, TOKEN_QUERY};
        use windows::Win32::System::Threading::OpenProcessToken;

        let dir = std::env::temp_dir();
        let cmd = Path::new(r"C:\Windows\System32\cmd.exe");
        let moniker = moniker_for("selftest");
        // `cmd /c pause` blocks on its (redirected, never-fed) stdin, so the
        // child stays alive while we inspect its token
        let mut sb = spawn(&moniker, cmd, &["/c".into(), "pause".into()], &dir, false)
            .expect("sandboxed spawn");

        let mut is_ac: u32 = 0;
        let mut ret_len: u32 = 0;
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(sb.process, TOKEN_QUERY, &mut token).expect("open token");
            GetTokenInformation(
                token,
                TokenIsAppContainer,
                Some(&mut is_ac as *mut _ as *mut _),
                size_of::<u32>() as u32,
                &mut ret_len,
            )
            .expect("token info");
            let _ = CloseHandle(token);
        }
        sb.kill();
        unsafe {
            let name = wide(&moniker);
            let _ = DeleteAppContainerProfile(PCWSTR(name.as_ptr()));
        }
        assert_eq!(is_ac, 1, "sandboxed child should be in an appcontainer");
    }

    // confirms a real plugin's stdio protocol survives the sandbox pipes: spawn a
    // plugin confined and read its `ready` handshake back. point TERMIE_TEST_PLUGIN
    // at a built plugin exe (plugins live in the termie-plugins repo). #[ignore]d
    // (profile + process side effects); run with `cargo test -- --ignored`
    #[test]
    #[ignore = "creates an appcontainer profile + process"]
    fn sandboxed_plugin_stdio_roundtrips() {
        use std::io::Read;
        use windows::Win32::Security::Isolation::DeleteAppContainerProfile;

        let Some(exe) = std::env::var_os("TERMIE_TEST_PLUGIN").map(std::path::PathBuf::from) else {
            eprintln!("skip: set TERMIE_TEST_PLUGIN to a built plugin exe");
            return;
        };
        if !exe.exists() {
            eprintln!("skip: TERMIE_TEST_PLUGIN not found: {}", exe.display());
            return;
        }
        let dir = exe.parent().unwrap();
        let moniker = moniker_for("selftest.tama");
        let mut sb = spawn(&moniker, &exe, &[], dir, false).expect("sandboxed spawn");
        let mut stdout = sb.take_stdout().expect("stdout");

        // read the first line; the plugin announces itself with a ready command
        let mut buf = [0u8; 256];
        let n = stdout.read(&mut buf).expect("read");
        let text = String::from_utf8_lossy(&buf[..n]);
        sb.kill();
        unsafe {
            let name = wide(&moniker);
            let _ = DeleteAppContainerProfile(PCWSTR(name.as_ptr()));
        }
        assert!(text.contains("\"ready\""), "expected a ready line, got: {text:?}");
    }
}
