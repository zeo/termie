//! the install/uninstall engine: file extraction, user PATH, shortcuts,
//! explorer verbs, the Add/Remove Programs entry, old-MSI migration, and the
//! self-removing uninstall. everything is per-user (HKCU + %LOCALAPPDATA%),
//! so no elevation is ever required — except removing a legacy per-machine
//! MSI, which msiexec prompts for on its own

use std::path::{Path, PathBuf};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM, WPARAM};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegEnumKeyExW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    KEY_READ, KEY_SET_VALUE, KEY_WRITE, REG_DWORD, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE,
    REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};

use crate::payload;

pub struct Options {
    pub add_path: bool,
    pub start_menu: bool,
    pub desktop: bool,
    pub context_menu: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { add_path: true, start_menu: true, desktop: false, context_menu: true }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn install_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA");
    PathBuf::from(base).join("Programs").join("termie")
}

pub fn exe_path() -> PathBuf {
    install_dir().join("termie.exe")
}

/// run the whole install; `progress(frac, label)` is called from this thread
pub fn install(opts: &Options, mut progress: impl FnMut(f32, &str)) -> Result<(), String> {
    let entries = payload::entries();
    if entries.is_empty() {
        return Err("this build carries no payload (dev binary)".into());
    }
    let dir = install_dir();

    // a legacy per-machine MSI shadows the new install; remove it first
    if let Some(product) = find_machine_msi() {
        progress(0.02, "removing the old MSI install");
        remove_msi(&product);
    }

    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    // a running termie holds a lock on writing but not on renaming: move the
    // live exe aside so the update lands, and clean the leftover next run
    let exe = dir.join("termie.exe");
    let old = dir.join("termie.exe.old");
    let _ = std::fs::remove_file(&old);
    if exe.exists() {
        let _ = std::fs::rename(&exe, &old);
    }

    let total: u64 = payload::installed_bytes(&entries).max(1);
    let mut done: u64 = 0;
    for e in &entries {
        progress(0.05 + 0.75 * (done as f32 / total as f32), &e.name);
        let raw = e.decompress()?;
        let target = dir.join(e.name.replace('/', "\\"));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, &raw).map_err(|err| format!("write {}: {err}", target.display()))?;
        done += e.raw_len;
    }

    // the uninstaller is this same program, parked in the install dir
    progress(0.82, "uninstaller");
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let uninst = dir.join("uninstall.exe");
    if me != uninst {
        let _ = std::fs::copy(&me, &uninst);
    }

    progress(0.86, "shortcuts");
    if opts.start_menu {
        let lnk = start_menu_lnk();
        let _ = write_shortcut(&lnk, &exe, &dir);
    }
    if opts.desktop {
        if let Some(lnk) = desktop_lnk() {
            let _ = write_shortcut(&lnk, &exe, &dir);
        }
    }

    progress(0.92, "registry");
    if opts.context_menu {
        register_verbs(&exe)?;
    }
    if opts.add_path {
        add_to_path(&dir)?;
    }
    register_arp(&dir, payload::installed_bytes(&entries))?;

    progress(1.0, "done");
    Ok(())
}

/// undo everything install() did, then schedule the directory's removal —
/// this exe lives inside it, so a detached shell deletes it after we exit
pub fn uninstall() -> Result<(), String> {
    let dir = install_dir();
    let _ = std::fs::remove_file(start_menu_lnk());
    if let Some(lnk) = desktop_lnk() {
        let _ = std::fs::remove_file(lnk);
    }
    unregister_verbs();
    remove_from_path(&dir);
    delete_hkcu_tree("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\termie");
    schedule_dir_removal(&dir);
    Ok(())
}

/// for /update: keep whatever integration choices the original install made
pub fn opts_from_existing() -> Options {
    let dir = install_dir();
    let dir_s = dir.to_string_lossy().to_string();
    let (path_now, _) = read_hkcu_path();
    let on_path = path_now
        .split(';')
        .any(|p| p.trim_end_matches('\\').eq_ignore_ascii_case(dir_s.trim_end_matches('\\')));
    let verb_present = unsafe {
        let mut key = HKEY::default();
        let p = wide("Software\\Classes\\Directory\\shell\\termie");
        let ok = RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()), Some(0), KEY_READ, &mut key)
            == ERROR_SUCCESS;
        if ok {
            let _ = RegCloseKey(key);
        }
        ok
    };
    Options {
        add_path: on_path,
        start_menu: start_menu_lnk().exists(),
        desktop: desktop_lnk().map(|l| l.exists()).unwrap_or(false),
        context_menu: verb_present,
    }
}

pub fn launch_app() {
    let exe = exe_path();
    let _ = std::process::Command::new(&exe).current_dir(install_dir()).spawn();
}

// ---- PATH ------------------------------------------------------------------

fn read_hkcu_path() -> (String, REG_VALUE_TYPE) {
    unsafe {
        let mut key = HKEY::default();
        let env = wide("Environment");
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(env.as_ptr()), Some(0), KEY_READ, &mut key)
            != ERROR_SUCCESS
        {
            return (String::new(), REG_EXPAND_SZ);
        }
        let name = wide("Path");
        let mut ty = REG_EXPAND_SZ;
        let mut buf = vec![0u16; 32 * 1024];
        let mut bytes = (buf.len() * 2) as u32;
        let q = RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut bytes),
        );
        let _ = RegCloseKey(key);
        if q != ERROR_SUCCESS {
            return (String::new(), REG_EXPAND_SZ);
        }
        let chars = (bytes as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..chars]);
        (s.trim_end_matches('\0').to_string(), ty)
    }
}

fn write_hkcu_path(value: &str, ty: REG_VALUE_TYPE) -> Result<(), String> {
    unsafe {
        let mut key = HKEY::default();
        let env = wide("Environment");
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(env.as_ptr()), Some(0), KEY_SET_VALUE, &mut key)
            != ERROR_SUCCESS
        {
            return Err("open HKCU\\Environment".into());
        }
        let name = wide("Path");
        let data = wide(value);
        let bytes = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
        let r = RegSetValueExW(key, PCWSTR(name.as_ptr()), None, ty, Some(bytes));
        let _ = RegCloseKey(key);
        if r != ERROR_SUCCESS {
            return Err("write HKCU\\Environment\\Path".into());
        }
        // tell running shells/explorer the environment changed
        let env_w = wide("Environment");
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(env_w.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            2000,
            None,
        );
        Ok(())
    }
}

fn add_to_path(dir: &Path) -> Result<(), String> {
    let dir_s = dir.to_string_lossy().to_string();
    let (cur, ty) = read_hkcu_path();
    let already = cur
        .split(';')
        .any(|p| p.trim_end_matches('\\').eq_ignore_ascii_case(dir_s.trim_end_matches('\\')));
    if already {
        return Ok(());
    }
    let joined = if cur.is_empty() { dir_s } else { format!("{cur};{dir_s}") };
    write_hkcu_path(&joined, ty)
}

fn remove_from_path(dir: &Path) {
    let dir_s = dir.to_string_lossy().to_string();
    let (cur, ty) = read_hkcu_path();
    if cur.is_empty() {
        return;
    }
    let kept: Vec<&str> = cur
        .split(';')
        .filter(|p| {
            !p.trim_end_matches('\\').eq_ignore_ascii_case(dir_s.trim_end_matches('\\'))
                && !p.trim().is_empty()
        })
        .collect();
    let _ = write_hkcu_path(&kept.join(";"), ty);
}

// ---- shortcuts ---------------------------------------------------------------

fn start_menu_lnk() -> PathBuf {
    let base = std::env::var_os("APPDATA").expect("APPDATA");
    PathBuf::from(base)
        .join("Microsoft\\Windows\\Start Menu\\Programs")
        .join("termie.lnk")
}

fn desktop_lnk() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")?;
    Some(PathBuf::from(home).join("Desktop").join("termie.lnk"))
}

fn write_shortcut(lnk: &Path, target: &Path, workdir: &Path) -> Result<(), String> {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::Interface;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| e.to_string())?;
        let t = wide(&target.to_string_lossy());
        let w = wide(&workdir.to_string_lossy());
        link.SetPath(PCWSTR(t.as_ptr())).map_err(|e| e.to_string())?;
        link.SetWorkingDirectory(PCWSTR(w.as_ptr())).map_err(|e| e.to_string())?;
        let pf: IPersistFile = link.cast().map_err(|e| e.to_string())?;
        let l = wide(&lnk.to_string_lossy());
        pf.Save(PCWSTR(l.as_ptr()), true).map_err(|e| e.to_string())
    }
}

// ---- registry: verbs + ARP ---------------------------------------------------

fn set_hkcu_sz(key: HKEY, name: Option<&str>, value: &str) {
    unsafe {
        let data = wide(value);
        let bytes = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
        let name_w = name.map(wide);
        let name_p = name_w.as_ref().map(|w| PCWSTR(w.as_ptr())).unwrap_or(PCWSTR::null());
        let _ = RegSetValueExW(key, name_p, None, REG_SZ, Some(bytes));
    }
}

fn create_hkcu(path: &str) -> Option<HKEY> {
    unsafe {
        let mut key = HKEY::default();
        let p = wide(path);
        let r = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(p.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        );
        (r == ERROR_SUCCESS).then_some(key)
    }
}

fn delete_hkcu_tree(path: &str) {
    unsafe {
        let p = wide(path);
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()));
    }
}

fn register_verbs(exe: &Path) -> Result<(), String> {
    let exe_s = exe.to_string_lossy();
    for root in [
        "Software\\Classes\\Directory\\shell\\termie",
        "Software\\Classes\\Directory\\Background\\shell\\termie",
    ] {
        let key = create_hkcu(root).ok_or("create verb key")?;
        set_hkcu_sz(key, None, "Open in termie");
        set_hkcu_sz(key, Some("Icon"), &format!("\"{exe_s}\""));
        unsafe {
            let _ = RegCloseKey(key);
        }
        let cmd = create_hkcu(&format!("{root}\\command")).ok_or("create verb command")?;
        set_hkcu_sz(cmd, None, &format!("\"{exe_s}\" --cwd \"%V\""));
        unsafe {
            let _ = RegCloseKey(cmd);
        }
    }
    // app paths makes `termie` resolvable from Run/Explorer even without PATH
    let app = create_hkcu(
        "Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\termie.exe",
    )
    .ok_or("create app paths")?;
    set_hkcu_sz(app, None, &exe_s);
    unsafe {
        let _ = RegCloseKey(app);
    }
    Ok(())
}

fn unregister_verbs() {
    delete_hkcu_tree("Software\\Classes\\Directory\\shell\\termie");
    delete_hkcu_tree("Software\\Classes\\Directory\\Background\\shell\\termie");
    delete_hkcu_tree("Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\termie.exe");
}

fn register_arp(dir: &Path, bytes: u64) -> Result<(), String> {
    let key = create_hkcu("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\termie")
        .ok_or("create ARP key")?;
    let exe = dir.join("termie.exe");
    set_hkcu_sz(key, Some("DisplayName"), "termie");
    set_hkcu_sz(key, Some("DisplayVersion"), payload::APP_VERSION);
    set_hkcu_sz(key, Some("Publisher"), "rot");
    set_hkcu_sz(key, Some("InstallLocation"), &dir.to_string_lossy());
    set_hkcu_sz(key, Some("DisplayIcon"), &exe.to_string_lossy());
    set_hkcu_sz(
        key,
        Some("UninstallString"),
        &format!("\"{}\" /uninstall", dir.join("uninstall.exe").to_string_lossy()),
    );
    unsafe {
        for name in ["NoModify", "NoRepair"] {
            let n = wide(name);
            let one: u32 = 1;
            let _ = RegSetValueExW(
                key,
                PCWSTR(n.as_ptr()),
                None,
                REG_DWORD,
                Some(&one.to_le_bytes()),
            );
        }
        let n = wide("EstimatedSize");
        let kb: u32 = (bytes / 1024).min(u32::MAX as u64) as u32;
        let _ = RegSetValueExW(key, PCWSTR(n.as_ptr()), None, REG_DWORD, Some(&kb.to_le_bytes()));
        let _ = RegCloseKey(key);
    }
    Ok(())
}

// ---- legacy MSI migration ----------------------------------------------------

/// the ProductCode of an old per-machine termie MSI, if one is installed
pub fn find_machine_msi() -> Option<String> {
    unsafe {
        for root in [
            "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
            "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
        ] {
            let mut key = HKEY::default();
            let p = wide(root);
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(p.as_ptr()), Some(0), KEY_READ, &mut key)
                != ERROR_SUCCESS
            {
                continue;
            }
            let mut idx = 0u32;
            loop {
                let mut name = [0u16; 256];
                let mut name_len = name.len() as u32;
                if RegEnumKeyExW(
                    key,
                    idx,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut name_len,
                    None,
                    None,
                    None,
                    None,
                ) != ERROR_SUCCESS
                {
                    break;
                }
                idx += 1;
                let sub: String = String::from_utf16_lossy(&name[..name_len as usize]);
                // MSI product keys are GUIDs; read its DisplayName
                let mut skey = HKEY::default();
                let sp = wide(&format!("{root}\\{sub}"));
                if RegOpenKeyExW(
                    HKEY_LOCAL_MACHINE,
                    PCWSTR(sp.as_ptr()),
                    Some(0),
                    KEY_READ,
                    &mut skey,
                ) != ERROR_SUCCESS
                {
                    continue;
                }
                let dn = wide("DisplayName");
                let mut ty = REG_SZ;
                let mut buf = [0u16; 256];
                let mut bytes = (buf.len() * 2) as u32;
                let q = RegQueryValueExW(
                    skey,
                    PCWSTR(dn.as_ptr()),
                    None,
                    Some(&mut ty),
                    Some(buf.as_mut_ptr() as *mut u8),
                    Some(&mut bytes),
                );
                let _ = RegCloseKey(skey);
                if q == ERROR_SUCCESS && ty == REG_SZ {
                    let chars = (bytes as usize / 2).min(buf.len());
                    let disp = String::from_utf16_lossy(&buf[..chars]);
                    if disp.trim_end_matches('\0') == "termie" && sub.starts_with('{') {
                        let _ = RegCloseKey(key);
                        return Some(sub);
                    }
                }
            }
            let _ = RegCloseKey(key);
        }
    }
    None
}

fn remove_msi(product: &str) {
    // try quietly first (works when this process is already elevated or the
    // MSI was per-user). if the product is still registered, re-run with a
    // UAC prompt via ShellExecute "runas" so the dual-Start-menu case can't
    // survive a successful native install
    let _ = std::process::Command::new("msiexec")
        .args(["/x", product, "/qn", "/norestart"])
        .status();
    if find_machine_msi().as_deref() == Some(product) {
        elevated_msiexec_uninstall(product);
    }
    // msiexec can leave the tree or the all-users shortcut when a prior
    // uninstall was interrupted; scrub whatever remains so Search only
    // shows the per-user copy
    scrub_machine_msi_leftovers();
}

fn elevated_msiexec_uninstall(product: &str) {
    use windows::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};
    use windows::Win32::Foundation::CloseHandle;

    let file = wide("msiexec.exe");
    let params = wide(&format!("/x {product} /qn /norestart"));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: windows::core::w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: 0, // SW_HIDE
        ..Default::default()
    };
    unsafe {
        if ShellExecuteExW(&mut info).is_ok() && !info.hProcess.is_invalid() {
            let _ = WaitForSingleObject(info.hProcess, INFINITE);
            let _ = CloseHandle(info.hProcess);
        }
    }
}

/// drop Program Files tree, all-users Start shortcut, and a machine PATH
/// entry left by the WiX MSI after its product key is gone
fn scrub_machine_msi_leftovers() {
    // best-effort: these paths need admin; when the elevated msiexec path
    // already removed them this is a no-op, and when it didn't we still
    // try so a partial cleanup doesn't leave Search with two entries
    let pf = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    let tree = pf.join("termie");
    if tree.is_dir() {
        let _ = std::fs::remove_dir_all(&tree);
    }
    if let Some(pd) = std::env::var_os("ProgramData") {
        let lnk = PathBuf::from(pd)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("termie.lnk");
        let _ = std::fs::remove_file(lnk);
    }
    scrub_machine_path_termie();
}

fn scrub_machine_path_termie() {
    unsafe {
        let mut key = HKEY::default();
        let sub = wide("SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment");
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(sub.as_ptr()),
            Some(0),
            KEY_READ | KEY_SET_VALUE,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return;
        }
        let name = wide("Path");
        let mut ty = REG_EXPAND_SZ;
        let mut bytes = 0u32;
        let q = RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            None,
            Some(&mut bytes),
        );
        if q != ERROR_SUCCESS || bytes == 0 {
            let _ = RegCloseKey(key);
            return;
        }
        let mut buf = vec![0u16; (bytes as usize / 2) + 1];
        let mut bytes2 = bytes;
        if RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut bytes2),
        ) != ERROR_SUCCESS
        {
            let _ = RegCloseKey(key);
            return;
        }
        let chars = (bytes2 as usize / 2).min(buf.len());
        let raw = String::from_utf16_lossy(&buf[..chars]);
        let path = raw.trim_end_matches('\0');
        let lower = path.to_ascii_lowercase();
        if !lower.contains("\\termie") && !lower.contains("/termie") {
            let _ = RegCloseKey(key);
            return;
        }
        let kept: Vec<&str> = path
            .split(';')
            .filter(|p| {
                let t = p.trim();
                if t.is_empty() {
                    return false;
                }
                let l = t.to_ascii_lowercase();
                // drop "…\termie" and "…\termie\" only — not unrelated paths
                // that merely contain the substring elsewhere
                !(l.ends_with("\\termie") || l.ends_with("/termie") || l.ends_with("\\termie\\") || l.ends_with("/termie/"))
            })
            .collect();
        let new_path = kept.join(";");
        let w = wide(&new_path);
        let _ = RegSetValueExW(
            key,
            PCWSTR(name.as_ptr()),
            Some(0),
            ty,
            Some(std::slice::from_raw_parts(
                w.as_ptr() as *const u8,
                (w.len() - 1) * 2,
            )),
        );
        let _ = RegCloseKey(key);
        // tell explorers to reload PATH
        let env = wide("Environment");
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(env.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            1000,
            None,
        );
    }
}

// ---- self-removal --------------------------------------------------------------

fn schedule_dir_removal(dir: &Path) {
    // this exe runs from inside `dir`, so removal happens after we exit: a
    // detached cmd waits a beat, then removes the tree
    let d = dir.to_string_lossy().to_string();
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let _ = std::process::Command::new("cmd")
        .raw_arg(format!("/c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{d}\""))
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn();
}

/// true when a termie.exe from this install dir currently has a window up —
/// cheap heuristic: the exe file is locked for delete but rename still works,
/// so instead ask the OS by trying to open the exe for exclusive write
pub fn app_seems_running() -> bool {
    let exe = exe_path();
    if !exe.exists() {
        return false;
    }
    std::fs::OpenOptions::new().write(true).open(&exe).is_err()
}

#[allow(dead_code)]
pub fn hwnd_null() -> HWND {
    HWND(std::ptr::null_mut())
}
