//! windows appcontainer launcher for plugin subprocesses.
//!
//! spawns a plugin inside an appcontainer so it runs at low integrity with no
//! access to the user's files, registry, network, windows, or other processes
//! unless a capability is granted. the sandbox is enabled by default;
//! `plugin_sandbox=off` uses the normal spawn path.
//!
//! network access uses the internetClient capability when permission is granted
//! only that plugin package sid gets read and execute access to its install directory

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::fs::MetadataExt;
use std::path::Path;

use crate::plugin::market::{system_executable, ProcessTree};
use windows::core::{Error as WinError, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, SetHandleInformation, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE,
    HANDLE_FLAGS, HANDLE_FLAG_INHERIT, HLOCAL,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GetNamedSecurityInfoW,
    REVOKE_ACCESS, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName, GetAppContainerFolderPath,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, FreeSid, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    ResumeThread, TerminateProcess, UpdateProcThreadAttribute, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    STARTUPINFOW,
};

/// the well-known capability sid for outbound network (internetClient)
const INTERNET_CLIENT_SID: &str = "S-1-15-3-1";
/// the legacy wildcard sid removed from plugin directories during migration
const ALL_APP_PACKAGES_SID: &str = "S-1-15-2-1";
/// SE_GROUP_ENABLED: mark a capability sid as enabled in the token
const SE_GROUP_ENABLED: u32 = 0x0000_0004;
const MONIKER_PREFIX: &str = "termie.plugin.";
const MAX_MONIKER_LEN: usize = 64;

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn to_io(e: WinError) -> io::Error {
    io::Error::other(e.message())
}

fn inheritable_null() -> io::Result<File> {
    let file = File::options().write(true).open("NUL")?;
    let handle = HANDLE(file.as_raw_handle());
    unsafe {
        SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT).map_err(to_io)?;
    }
    Ok(file)
}

/// a plugin process running inside an appcontainer plus the host ends of its
/// stdio pipes. dropping or `kill`ing it stops the process
pub struct Sandboxed {
    process: HANDLE,
    tree: ProcessTree,
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
                self.tree.terminate();
                let _ = TerminateProcess(self.process, 1);
                let _ = CloseHandle(self.process);
            }
            self.process = HANDLE::default();
        }
    }
}

impl Drop for Sandboxed {
    fn drop(&mut self) {
        self.kill();
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
    let app = wide(&program.to_string_lossy());
    let mut cmdline = build_cmdline(program, args);
    let cwd = wide(&dir.to_string_lossy());
    let name = wide(moniker);
    let child_stderr_file = inheritable_null()?;
    let child_stderr = HANDLE(child_stderr_file.as_raw_handle());

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
        if let Err(error) = grant_appcontainer(dir, app_sid) {
            free_sids(app_sid, PSID::default());
            return Err(error);
        }
        let environment = match plugin_environment(app_sid) {
            Ok(environment) => environment,
            Err(error) => {
                free_sids(app_sid, PSID::default());
                return Err(error);
            }
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

        // process attributes carry the sandbox token and child pipes
        let mut size: usize = 0;
        let _ = InitializeProcThreadAttributeList(None, 2, Some(0), &mut size);
        let mut attr_buf = vec![0u8; size];
        let attr_list = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr() as *mut _);
        if let Err(e) = InitializeProcThreadAttributeList(Some(attr_list), 2, Some(0), &mut size) {
            close_all(&[child_stdin_r, host_stdin_w, host_stdout_r, child_stdout_w]);
            free_sids(app_sid, cap_sid);
            return Err(to_io(e));
        }
        let mut child_handles = [child_stdin_r, child_stdout_w, child_stderr];
        if let Err(e) = UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(child_handles.as_mut_ptr().cast()),
            size_of::<HANDLE>() * child_handles.len(),
            None,
            None,
        ) {
            DeleteProcThreadAttributeList(attr_list);
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
        si.StartupInfo.hStdError = child_stderr;
        let mut pi = PROCESS_INFORMATION::default();
        let tree = match ProcessTree::new_kill_on_close() {
            Ok(tree) => tree,
            Err(error) => {
                DeleteProcThreadAttributeList(attr_list);
                close_all(&[child_stdin_r, host_stdin_w, host_stdout_r, child_stdout_w]);
                free_sids(app_sid, cap_sid);
                return Err(error);
            }
        };
        let res = CreateProcessW(
            PCWSTR(app.as_ptr()),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            true,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_NO_WINDOW
                | CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT,
            Some(environment.as_ptr().cast()),
            PCWSTR(cwd.as_ptr()),
            &si as *const STARTUPINFOEXW as *const STARTUPINFOW,
            &mut pi,
        );
        drop(child_stderr_file);

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
        let started = tree.assign_handle(pi.hProcess).and_then(|_| {
            if ResumeThread(pi.hThread) == u32::MAX {
                Err(to_io(WinError::from_thread()))
            } else {
                Ok(())
            }
        });
        if let Err(error) = started {
            let _ = TerminateProcess(pi.hProcess, 1);
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(host_stdin_w);
            let _ = CloseHandle(host_stdout_r);
            return Err(error);
        }
        let _ = CloseHandle(pi.hThread);

        let stdin = File::from_raw_handle(host_stdin_w.0 as *mut _);
        let stdout = File::from_raw_handle(host_stdout_r.0 as *mut _);
        Ok(Sandboxed {
            process: pi.hProcess,
            tree,
            stdin: Some(stdin),
            stdout: Some(stdout),
        })
    }
}

fn plugin_environment(app_sid: PSID) -> io::Result<Vec<u16>> {
    let comspec = system_executable("cmd")
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "windows system directory unavailable",
            )
        })?;
    let system_dir = comspec
        .parent()
        .ok_or_else(|| io::Error::other("windows system directory has no parent"))?;
    let system_root = system_dir
        .parent()
        .ok_or_else(|| io::Error::other("windows system root unavailable"))?;
    let local_app_data = appcontainer_folder(app_sid)?;
    std::fs::create_dir_all(local_app_data.join("Temp"))?;
    Ok(build_environment_block(
        system_root,
        system_dir,
        &comspec,
        &local_app_data,
    ))
}

fn sid_string(sid: PSID) -> io::Result<String> {
    unsafe {
        let mut sid_text = PWSTR::null();
        ConvertSidToStringSidW(sid, &mut sid_text).map_err(to_io)?;
        let text = sid_text
            .to_string()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        let _ = LocalFree(Some(HLOCAL(sid_text.0.cast())));
        text
    }
}

fn appcontainer_folder(app_sid: PSID) -> io::Result<std::path::PathBuf> {
    unsafe {
        let sid_text = wide(&sid_string(app_sid)?);
        let folder = GetAppContainerFolderPath(PCWSTR(sid_text.as_ptr()));
        let folder = folder.map_err(to_io)?;
        let path = folder
            .to_string()
            .map(std::path::PathBuf::from)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        CoTaskMemFree(Some(folder.0.cast()));
        path
    }
}

fn build_environment_block(
    system_root: &Path,
    system_dir: &Path,
    comspec: &Path,
    local_app_data: &Path,
) -> Vec<u16> {
    let temp = local_app_data.join("Temp");
    let mut vars = [
        ("SystemRoot", system_root.as_os_str()),
        ("WINDIR", system_root.as_os_str()),
        ("ComSpec", comspec.as_os_str()),
        ("PATH", system_dir.as_os_str()),
        ("PATHEXT", OsStr::new(".COM;.EXE;.BAT;.CMD")),
        ("LOCALAPPDATA", local_app_data.as_os_str()),
        ("TEMP", temp.as_os_str()),
        ("TMP", temp.as_os_str()),
    ];
    vars.sort_unstable_by_key(|(name, _)| *name);

    let mut block = Vec::new();
    for (name, value) in vars {
        block.extend(name.encode_utf16());
        block.push('=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
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

fn sid_from_string(text: &str) -> io::Result<PSID> {
    let mut sid = PSID::default();
    let text = wide(text);
    unsafe {
        ConvertStringSidToSidW(PCWSTR(text.as_ptr()), &mut sid).map_err(to_io)?;
    }
    Ok(sid)
}

fn plugin_tree_paths(
    dir: &Path,
    reject_reparse_points: bool,
) -> io::Result<(Vec<(std::path::PathBuf, bool)>, bool)> {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    let root = std::fs::symlink_metadata(dir)?;
    if !root.is_dir() || root.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "plugin directory is not a local directory",
        ));
    }
    let mut paths = vec![(dir.to_path_buf(), true)];
    let mut pending = vec![dir.to_path_buf()];
    let mut skipped_reparse_point = false;
    while let Some(parent) = pending.pop() {
        for entry in std::fs::read_dir(parent)? {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                if reject_reparse_points {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "plugin directory contains a reparse point",
                    ));
                }
                skipped_reparse_point = true;
                continue;
            }
            let is_dir = metadata.is_dir();
            if is_dir {
                pending.push(path.clone());
            }
            paths.push((path, is_dir));
        }
    }
    Ok((paths, skipped_reparse_point))
}

fn change_path_access(
    path: &Path,
    is_dir: bool,
    changes: &[(PSID, windows::Win32::Security::Authorization::ACCESS_MODE, u32)],
) -> io::Result<()> {
    let mut path = wide(&path.to_string_lossy());
    let mut old_acl = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let mut new_acl = std::ptr::null_mut();
    let inheritance =
        if is_dir { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { Default::default() };
    let entries: Vec<_> = changes
        .iter()
        .map(|&(sid, mode, permissions)| EXPLICIT_ACCESS_W {
            grfAccessPermissions: permissions,
            grfAccessMode: mode,
            grfInheritance: inheritance,
            Trustee: TRUSTEE_W {
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: PWSTR(sid.0.cast()),
                ..Default::default()
            },
        })
        .collect();

    let result = unsafe {
        let status = GetNamedSecurityInfoW(
            PCWSTR(path.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_acl),
            None,
            &mut descriptor,
        );
        if status != ERROR_SUCCESS {
            Err(io::Error::from_raw_os_error(status.0 as i32))
        } else {
            let status = SetEntriesInAclW(Some(&entries), Some(old_acl), &mut new_acl);
            if status != ERROR_SUCCESS {
                Err(io::Error::from_raw_os_error(status.0 as i32))
            } else {
                let status = SetNamedSecurityInfoW(
                    PWSTR(path.as_mut_ptr()),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(new_acl),
                    None,
                );
                if status == ERROR_SUCCESS {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(status.0 as i32))
                }
            }
        }
    };
    unsafe {
        if !new_acl.is_null() {
            let _ = LocalFree(Some(HLOCAL(new_acl.cast())));
        }
        if !descriptor.0.is_null() {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
    }
    result
}

fn change_tree_access(
    dir: &Path,
    changes: &[(PSID, windows::Win32::Security::Authorization::ACCESS_MODE, u32)],
    reject_reparse_points: bool,
) -> io::Result<bool> {
    let (paths, skipped_reparse_point) =
        plugin_tree_paths(dir, reject_reparse_points)?;
    for (path, is_dir) in paths {
        change_path_access(&path, is_dir, changes)?;
    }
    Ok(skipped_reparse_point)
}

/// grant only this appcontainer read+execute on its plugin directory
fn grant_appcontainer(dir: &Path, app_sid: PSID) -> io::Result<()> {
    let legacy_sid = sid_from_string(ALL_APP_PACKAGES_SID)?;
    let permissions = FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0;
    let result = change_tree_access(
        dir,
        &[(legacy_sid, REVOKE_ACCESS, 0), (app_sid, SET_ACCESS, permissions)],
        true,
    );
    unsafe {
        let _ = LocalFree(Some(HLOCAL(legacy_sid.0)));
    }
    result.map(|_| ())
}

/// remove the legacy wildcard appcontainer ace from a plugin directory
pub fn remove_legacy_access(dir: &Path) -> io::Result<()> {
    let legacy_sid = sid_from_string(ALL_APP_PACKAGES_SID)?;
    let result = change_tree_access(dir, &[(legacy_sid, REVOKE_ACCESS, 0)], false);
    unsafe {
        let _ = LocalFree(Some(HLOCAL(legacy_sid.0)));
    }
    match result {
        Ok(false) => Ok(()),
        Ok(true) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "legacy access remains on a skipped reparse point",
        )),
        Err(error) => Err(error),
    }
}

/// an appcontainer moniker derived from an already validated plugin id
pub fn moniker_for(id: &str) -> String {
    let direct = format!("{MONIKER_PREFIX}{id}");
    if direct.len() <= MAX_MONIKER_LEN {
        return direct;
    }
    let digest = crate::update::sha256_hex(id.as_bytes());
    let suffix = &digest[..32];
    let visible = MAX_MONIKER_LEN - MONIKER_PREFIX.len() - suffix.len() - 1;
    format!("{MONIKER_PREFIX}{}-{suffix}", &id[..visible])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moniker_is_bounded_and_prefixed() {
        assert_eq!(moniker_for("pet"), "termie.plugin.pet");
        let longest_direct = "x".repeat(MAX_MONIKER_LEN - MONIKER_PREFIX.len());
        assert_eq!(
            moniker_for(&longest_direct),
            format!("{MONIKER_PREFIX}{longest_direct}")
        );
        let first = format!("{}a", "x".repeat(63));
        let second = format!("{}b", "x".repeat(63));
        assert_eq!(moniker_for(&first).len(), 64);
        assert_ne!(moniker_for(&first), moniker_for(&second));
        assert_eq!(
            moniker_for(&first),
            "termie.plugin.xxxxxxxxxxxxxxxxx-2fc161cbc92f96faebbbb9dd80287517"
        );
    }

    #[test]
    fn null_stderr_handle_is_inheritable() {
        use windows::Win32::Foundation::GetHandleInformation;

        let file = inheritable_null().expect("open NUL");
        let mut flags = 0;
        unsafe {
            GetHandleInformation(HANDLE(file.as_raw_handle()), &mut flags)
                .expect("inspect NUL handle");
        }
        assert_ne!(flags & HANDLE_FLAG_INHERIT.0, 0);
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

    #[test]
    fn plugin_environment_is_minimal_sorted_and_double_nul_terminated() {
        let block = build_environment_block(
            Path::new(r"C:\Windows"),
            Path::new(r"C:\Windows\System32"),
            Path::new(r"C:\Windows\System32\cmd.exe"),
            Path::new(r"C:\Users\test\AppData\Local\Packages\termie.plugin.test\AC"),
        );
        assert_eq!(&block[block.len() - 2..], &[0, 0]);

        let entries: Vec<_> = block[..block.len() - 1]
            .split(|unit| *unit == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| String::from_utf16(entry).unwrap())
            .collect();
        assert_eq!(
            entries,
            [
                r"ComSpec=C:\Windows\System32\cmd.exe",
                r"LOCALAPPDATA=C:\Users\test\AppData\Local\Packages\termie.plugin.test\AC",
                r"PATH=C:\Windows\System32",
                "PATHEXT=.COM;.EXE;.BAT;.CMD",
                r"SystemRoot=C:\Windows",
                r"TEMP=C:\Users\test\AppData\Local\Packages\termie.plugin.test\AC\Temp",
                r"TMP=C:\Users\test\AppData\Local\Packages\termie.plugin.test\AC\Temp",
                r"WINDIR=C:\Windows",
            ]
        );
        assert!(
            entries
                .iter()
                .all(|entry| !entry.starts_with("GITHUB_TOKEN=")
                    && !entry.starts_with("USERPROFILE="))
        );
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

        let dir = std::env::temp_dir().join(format!(
            "termie-appcontainer-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir(&dir).expect("create test directory");
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
        let _ = std::fs::remove_dir(&dir);
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
        let dir = std::env::temp_dir().join(format!(
            "termie-plugin-stdio-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir(&dir).expect("create test directory");
        let copied = dir.join("plugin.exe");
        std::fs::copy(&exe, &copied).expect("copy plugin fixture");
        let moniker = moniker_for("selftest.tama");
        let outcome = (|| -> io::Result<String> {
            let mut sb = spawn(&moniker, &copied, &[], &dir, false)?;
            let mut stdout = sb.take_stdout().expect("stdout");
            let mut buf = [0u8; 256];
            let n = stdout.read(&mut buf)?;
            sb.kill();
            Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
        })();
        unsafe {
            let name = wide(&moniker);
            let _ = DeleteAppContainerProfile(PCWSTR(name.as_ptr()));
        }
        let _ = std::fs::remove_file(&copied);
        let _ = std::fs::remove_dir(&dir);
        let text = outcome.expect("sandboxed stdio");
        assert!(text.contains("\"ready\""), "expected a ready line, got: {text:?}");
    }
}
