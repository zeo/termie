//! windows-specific window effects: rounded corners (DWM) + flat per-pixel
//! window opacity (premultiplied alpha — not acrylic/mica)

/// suppress the OS "application was unable to start correctly" / crash dialogs
/// for this process and the children it spawns. without this, a pre-warmed pool
/// pwsh that loses its ConPTY during termie's exit pops a 0xc0000142 dialog.
/// child processes inherit the error mode set here.
#[cfg(windows)]
pub fn suppress_child_error_dialogs() {
    // SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX
    unsafe extern "system" {
        fn SetErrorMode(mode: u32) -> u32;
    }
    unsafe {
        SetErrorMode(0x0001 | 0x0002);
    }
}

#[cfg(not(windows))]
pub fn suppress_child_error_dialogs() {}

/// app / taskbar / alt-tab icon as RGBA: the ">_<" mark, decoded from the 1024
/// master (assets/icon.png) into a 128x128 raw RGBA blob at build time so we
/// stay free of any image-decoding dependency at runtime
pub fn app_icon() -> (Vec<u8>, u32, u32) {
    const N: u32 = 128;
    let rgba = include_bytes!("../assets/icon_128.rgba");
    (rgba.to_vec(), N, N)
}

#[cfg(windows)]
pub fn apply_window_effects(hwnd_handle: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
        DWM_WINDOW_CORNER_PREFERENCE,
    };

    let hwnd = HWND(hwnd_handle as *mut core::ffi::c_void);
    unsafe {
        // rounded corners + shadow even though we're undecorated
        let corner: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
    }
}

#[cfg(not(windows))]
pub fn apply_window_effects(_hwnd_handle: isize) {}

/// mark a window non-activating (WS_EX_NOACTIVATE): it can never take
/// keyboard focus, by click or programmatically. --drive fixtures use this so
/// an automated run can't steal focus from (or feed keys to) the user
#[cfg(windows)]
pub fn set_no_activate(hwnd_handle: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
    };

    let hwnd = HWND(hwnd_handle as *mut core::ffi::c_void);
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize);
    }
}

#[cfg(not(windows))]
pub fn set_no_activate(_hwnd_handle: isize) {}

/// opt the window into the Win11 system backdrop (Mica) so the desktop shows
/// through the chrome. cosmetic only: on Win10 and early Win11 the attribute
/// is unknown and the call fails, which is ignored — the window keeps its
/// rounded corners and flat opacity
#[cfg(windows)]
pub fn apply_backdrop(hwnd_handle: isize, on: bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};

    // DWMWA_SYSTEMBACKDROP_TYPE (38) with DWMSBT_MAINWINDOW (2, mica) or
    // DWMSBT_NONE (1) to turn it back off; the attribute number is spelled raw
    // because not every windows-rs release exports the enum
    const DWMWA_SYSTEMBACKDROP_TYPE: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(38);
    let backdrop: i32 = if on { 2 } else { 1 };
    let hwnd = HWND(hwnd_handle as *mut core::ffi::c_void);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
    }
}

#[cfg(not(windows))]
pub fn apply_backdrop(_hwnd_handle: isize, _on: bool) {}

/// reflect OSC 9;4 progress on the window's taskbar button. state: 0 clear,
/// 1 normal (green), 2 error (red), 3 indeterminate (pulse), 4 paused (yellow);
/// pct is 0–100 and ignored for clear/indeterminate. failures are swallowed —
/// taskbar flair must never take the terminal down
#[cfg(windows)]
pub fn set_taskbar_progress(hwnd_handle: isize, state: u8, pct: u8) {
    use std::cell::OnceCell;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        ITaskbarList3, TaskbarList, TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL,
        TBPF_PAUSED,
    };
    thread_local! {
        // created once per thread; None caches a failed creation so we don't
        // retry com on every output chunk
        static TASKBAR: OnceCell<Option<ITaskbarList3>> = const { OnceCell::new() };
    }
    let hwnd = HWND(hwnd_handle as *mut core::ffi::c_void);
    TASKBAR.with(|cell| {
        let tb = cell.get_or_init(|| unsafe {
            // winit already initialized com (ole drag-drop) on this thread;
            // S_FALSE / RPC_E_CHANGED_MODE here are both fine to ignore
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok()
        });
        let Some(tb) = tb else { return };
        let flags = match state {
            1 => TBPF_NORMAL,
            2 => TBPF_ERROR,
            3 => TBPF_INDETERMINATE,
            4 => TBPF_PAUSED,
            _ => TBPF_NOPROGRESS,
        };
        unsafe {
            let _ = tb.SetProgressState(hwnd, flags);
            if matches!(state, 1 | 2 | 4) {
                let _ = tb.SetProgressValue(hwnd, pct.min(100) as u64, 100);
            }
        }
    });
}

#[cfg(not(windows))]
pub fn set_taskbar_progress(_hwnd_handle: isize, _state: u8, _pct: u8) {}

/// populate the taskbar jump list: right-clicking the (pinned) icon offers a
/// plain new window plus one per shell and custom profile, launching the exe
/// with `--shell <name>`. failures are swallowed — the jump list is decoration,
/// never worth blocking startup or crashing over
#[cfg(windows)]
pub fn update_jumplist(tasks: &[(String, String)]) {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
    use windows::Win32::System::Variant::VT_LPWSTR;
    use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{
        DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW,
        SHStrDupW, ShellLink,
    };
    use windows::core::{GUID, Interface, PCWSTR};
    // System.Title — the string the jump list displays for a task
    const PKEY_TITLE: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0xF29F85E0_4FF9_1068_AB91_08002B27B3D9),
        pid: 2,
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let exe: Vec<u16> = exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let result: windows::core::Result<()> = (|| unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let list: ICustomDestinationList =
            CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
        let mut slots = 0u32;
        let _removed: IObjectArray = list.BeginList(&mut slots)?;
        let coll: IObjectCollection =
            CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;
        for (title, args) in tasks {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(PCWSTR(exe.as_ptr()))?;
            let args: Vec<u16> = args.encode_utf16().chain(std::iter::once(0)).collect();
            link.SetArguments(PCWSTR(args.as_ptr()))?;
            link.SetIconLocation(PCWSTR(exe.as_ptr()), 0)?;
            let store: IPropertyStore = link.cast()?;
            // System.Title, or AddUserTasks drops the task. the string must be a
            // CoTaskMem allocation the propvariant owns: SHStrDupW makes one,
            // SetValue copies it, and PropVariantClear frees our copy — pointing
            // the propvariant at a Rust buffer corrupts the heap when the store
            // clears it
            let wtitle: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
            let mut pv = PROPVARIANT::default();
            {
                let inner = &mut *pv.Anonymous.Anonymous;
                inner.vt = VT_LPWSTR;
                inner.Anonymous.pwszVal = SHStrDupW(PCWSTR(wtitle.as_ptr()))?;
            }
            let set = store.SetValue(&PKEY_TITLE, &pv);
            let _ = PropVariantClear(&mut pv);
            set?;
            store.Commit()?;
            coll.AddObject(&link)?;
        }
        list.AddUserTasks(&coll.cast::<IObjectArray>()?)?;
        list.CommitList()?;
        Ok(())
    })();
    if let Err(e) = result {
        log::debug!("jump list update failed: {e}");
    }
}

#[cfg(not(windows))]
pub fn update_jumplist(_tasks: &[(String, String)]) {}

/// flash the window's taskbar button until it regains the foreground — the
/// standard "needs attention" signal for a bell in an unfocused window
#[cfg(windows)]
pub fn flash_taskbar(hwnd_handle: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FlashWindowEx, FLASHWINFO, FLASHW_TIMERNOFG, FLASHW_TRAY,
    };
    let fi = FLASHWINFO {
        cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
        hwnd: HWND(hwnd_handle as *mut core::ffi::c_void),
        dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
        uCount: 0,
        dwTimeout: 0,
    };
    unsafe {
        let _ = FlashWindowEx(&fi);
    }
}

#[cfg(not(windows))]
pub fn flash_taskbar(_hwnd_handle: isize) {}

/// the user's "roll the wheel to scroll N lines" setting; u32::MAX is the
/// "one screen at a time" sentinel (WHEEL_PAGESCROLL)
#[cfg(windows)]
pub fn wheel_scroll_lines() -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETWHEELSCROLLLINES, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };
    let mut lines: u32 = 3;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            Some(&mut lines as *mut u32 as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() { 3 } else { lines }
}

#[cfg(not(windows))]
pub fn wheel_scroll_lines() -> u32 {
    3
}

#[cfg(windows)]
pub fn local_hm() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    format!("{:02}:{:02}", st.wHour, st.wMinute)
}

#[cfg(not(windows))]
pub fn local_hm() -> String {
    String::new()
}

/// open an http(s) URL in the default browser via the shell. the scheme is
/// re-checked here so only web links can ever be launched, never a file path
/// or a custom protocol handler that could start an arbitrary app
#[cfg(windows)]
pub fn open_url(url: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return;
    }
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(not(windows))]
pub fn open_url(_url: &str) {}

/// small registry helpers for the default-terminal registration: read and
/// write REG_SZ values under HKCU. every step is fallible and never panics
#[cfg(windows)]
mod reg {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW,
    };
    use windows::core::{PCWSTR, PWSTR};

    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// read an HKCU REG_SZ value; None when the key/value is missing
    pub fn read_sz(path: &str, value: &str) -> Option<String> {
        let path = wide(path);
        let value = wide(value);
        unsafe {
            let mut key = HKEY::default();
            if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), Some(0), KEY_READ, &mut key)
                != ERROR_SUCCESS
            {
                return None;
            }
            let mut buf = [0u16; 512];
            let mut bytes = (buf.len() * 2) as u32;
            let mut ty = REG_SZ;
            let q = RegQueryValueExW(
                key,
                PCWSTR(value.as_ptr()),
                None,
                Some(&mut ty),
                Some(buf.as_mut_ptr() as *mut u8),
                Some(&mut bytes),
            );
            let _ = RegCloseKey(key);
            if q != ERROR_SUCCESS || ty != REG_SZ {
                return None;
            }
            let chars = (bytes as usize / 2).min(buf.len());
            Some(String::from_utf16_lossy(&buf[..chars]).trim_end_matches('\0').to_string())
        }
    }

    /// create-or-open an HKCU key and set a REG_SZ value ("" = default value)
    pub fn write_sz(path: &str, value: &str, data: &str) -> bool {
        let path = wide(path);
        let value = wide(value);
        let data = wide(data);
        unsafe {
            let mut key = HKEY::default();
            if RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(path.as_ptr()),
                None,
                PWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut key,
                None,
            ) != ERROR_SUCCESS
            {
                return false;
            }
            let bytes: &[u8] = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
            let name = if value.len() <= 1 { PCWSTR::null() } else { PCWSTR(value.as_ptr()) };
            let r = RegSetValueExW(key, name, None, REG_SZ, Some(bytes));
            let _ = RegCloseKey(key);
            r == ERROR_SUCCESS
        }
    }

    /// delete an HKCU key and everything under it
    pub fn delete_tree(path: &str) {
        let path = wide(path);
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()));
        }
    }
}

/// where windows stores the default-terminal choice
#[cfg(windows)]
const DELEGATION_KEY: &str = "Console\\%%Startup";
#[cfg(windows)]
const CLSID_KEY: &str = "Software\\Classes\\CLSID\\{D6F7E8A1-3C52-4B0F-9E6A-71B2C0A4F3D9}";
/// "let windows decide", the OS default when nothing is registered
#[cfg(windows)]
const DELEGATION_DEFAULT: &str = "{00000000-0000-0000-0000-000000000000}";

#[cfg(windows)]
fn termie_clsid_string() -> String {
    format!("{{{:?}}}", crate::defterm::CLSID_TERMIE_HANDOFF)
}

/// true when the user's default terminal is termie
#[cfg(windows)]
pub fn defterm_registered() -> bool {
    reg::read_sz(DELEGATION_KEY, "DelegationTerminal")
        .map(|v| v.eq_ignore_ascii_case(&termie_clsid_string()))
        .unwrap_or(false)
}

/// make termie the default terminal: register the COM local server for the
/// handoff class and point the console delegation pair at OpenConsole + termie.
/// the previous pair is stashed alongside so unregistering restores the user's
/// old choice instead of guessing
#[cfg(windows)]
pub fn register_defterm() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let server = format!("\"{}\" -Embedding", exe.display());
    let prev_console = reg::read_sz(DELEGATION_KEY, "DelegationConsole").unwrap_or_default();
    let prev_terminal = reg::read_sz(DELEGATION_KEY, "DelegationTerminal").unwrap_or_default();
    let ok = reg::write_sz(CLSID_KEY, "", "termie terminal handoff")
        && reg::write_sz(&format!("{CLSID_KEY}\\LocalServer32"), "", &server)
        && reg::write_sz(DELEGATION_KEY, "DelegationConsole", crate::defterm::CLSID_OPENCONSOLE)
        && reg::write_sz(DELEGATION_KEY, "DelegationTerminal", &termie_clsid_string());
    if ok && !prev_terminal.eq_ignore_ascii_case(&termie_clsid_string()) {
        // remembered under our own key, harmless if it lingers
        let _ = reg::write_sz(CLSID_KEY, "PrevDelegationConsole", &prev_console);
        let _ = reg::write_sz(CLSID_KEY, "PrevDelegationTerminal", &prev_terminal);
    }
    ok
}

/// stop being the default terminal: restore the previous delegation pair (or
/// the OS default) and drop the COM registration
#[cfg(windows)]
pub fn unregister_defterm() -> bool {
    let prev_console = reg::read_sz(CLSID_KEY, "PrevDelegationConsole").filter(|s| !s.is_empty());
    let prev_terminal = reg::read_sz(CLSID_KEY, "PrevDelegationTerminal").filter(|s| !s.is_empty());
    let ok = reg::write_sz(
        DELEGATION_KEY,
        "DelegationConsole",
        prev_console.as_deref().unwrap_or(DELEGATION_DEFAULT),
    ) && reg::write_sz(
        DELEGATION_KEY,
        "DelegationTerminal",
        prev_terminal.as_deref().unwrap_or(DELEGATION_DEFAULT),
    );
    reg::delete_tree(CLSID_KEY);
    ok
}

/// keep the COM server path current: after an update or a moved install, the
/// registration must point at the exe that is actually running. a cargo build
/// tree is exempt — running a dev binary once must not hijack the delegation
/// onto a path that the next rebuild locks or replaces
#[cfg(windows)]
pub fn refresh_defterm_server_path() {
    if !defterm_registered() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let dev_tree = exe.ancestors().any(|p| {
            p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("target")) || p.join("Cargo.toml").is_file()
        });
        if dev_tree {
            return;
        }
        let server = format!("\"{}\" -Embedding", exe.display());
        let _ = reg::write_sz(&format!("{CLSID_KEY}\\LocalServer32"), "", &server);
    }
}

/// true when this process holds an elevated (admin) token; cached — the
/// answer can't change for the life of a process
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    static ELEVATED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ELEVATED.get_or_init(|| unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() {
            return false;
        }
        let mut info = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            tok,
            TokenElevation,
            Some(&mut info as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(tok);
        ok && info.TokenIsElevated != 0
    })
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

/// relaunch termie elevated through the UAC prompt with `args` as its command
/// line; false when the user declined or the launch failed
#[cfg(windows)]
pub fn launch_elevated(args: &str) -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe_w = reg::wide(&exe.display().to_string());
    let args_w = reg::wide(args);
    let h = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR(args_w.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // shellapi contract: values above 32 are success
    h.0 as usize > 32
}

#[cfg(not(windows))]
pub fn launch_elevated(_args: &str) -> bool {
    false
}

// the CLSID appears twice — as a GUID for COM and as registry-path text — and
// the two must never drift apart
#[cfg(all(test, windows))]
mod defterm_reg_tests {
    #[test]
    fn clsid_key_matches_the_guid() {
        let key = super::CLSID_KEY.to_ascii_uppercase();
        let clsid = super::termie_clsid_string().to_ascii_uppercase();
        assert!(key.ends_with(&clsid), "{key} vs {clsid}");
    }
}

#[cfg(not(windows))]
pub fn defterm_registered() -> bool {
    false
}
#[cfg(not(windows))]
pub fn register_defterm() -> bool {
    false
}
#[cfg(not(windows))]
pub fn unregister_defterm() -> bool {
    false
}
#[cfg(not(windows))]
pub fn refresh_defterm_server_path() {}

/// the installed WSL distribution friendly-names, read from the registry
/// (HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss), the same source
/// Windows Terminal uses. empty on any failure or when WSL isn't installed —
/// every step is fallible and the whole thing never panics
#[cfg(windows)]
pub fn wsl_distros() -> Vec<String> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, REG_SZ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
        RegQueryValueExW,
    };
    use windows::core::{PCWSTR, PWSTR};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let mut out: Vec<String> = Vec::new();
    let lxss_path = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Lxss");
    let dist_value = wide("DistributionName");
    unsafe {
        let mut lxss = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(lxss_path.as_ptr()), Some(0), KEY_READ, &mut lxss)
            != ERROR_SUCCESS
        {
            return out; // no WSL installed
        }
        let mut idx = 0u32;
        loop {
            // each subkey is a distro guid; its DistributionName value is the name
            let mut name = [0u16; 256];
            let mut name_len = name.len() as u32;
            let r = RegEnumKeyExW(lxss, idx, Some(PWSTR(name.as_mut_ptr())), &mut name_len, None, None, None, None);
            if r != ERROR_SUCCESS {
                break; // ERROR_NO_MORE_ITEMS or any error ends the scan
            }
            idx += 1;
            let mut guid: Vec<u16> = name[..(name_len as usize).min(name.len())].to_vec();
            guid.push(0);
            let mut dkey = HKEY::default();
            if RegOpenKeyExW(lxss, PCWSTR(guid.as_ptr()), Some(0), KEY_READ, &mut dkey) != ERROR_SUCCESS {
                continue;
            }
            let mut buf = [0u16; 256];
            let mut bytes = (buf.len() * 2) as u32;
            let mut ty = REG_SZ;
            let q = RegQueryValueExW(
                dkey,
                PCWSTR(dist_value.as_ptr()),
                None,
                Some(&mut ty),
                Some(buf.as_mut_ptr() as *mut u8),
                Some(&mut bytes),
            );
            let _ = RegCloseKey(dkey);
            if q == ERROR_SUCCESS && ty == REG_SZ {
                let chars = (bytes as usize / 2).min(buf.len());
                let s = String::from_utf16_lossy(&buf[..chars]);
                let s = s.trim_end_matches('\0').trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
            }
        }
        let _ = RegCloseKey(lxss);
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(not(windows))]
pub fn wsl_distros() -> Vec<String> {
    Vec::new()
}

/// show a blocking error dialog. used only on the fatal boot path (gpu init
/// failed and we are about to exit) so the user sees why rather than a window
/// that silently never appears. never call this per-frame
#[cfg(windows)]
pub fn show_fatal_error(msg: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
    };
    use windows::core::PCWSTR;
    // both buffers must outlive the modal call and be nul-terminated
    let body: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let caption: Vec<u16> = "termie \u{2014} GPU initialization failed\0".encode_utf16().collect();
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(body.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR | MB_TOPMOST | MB_SETFOREGROUND,
        );
    }
}

#[cfg(not(windows))]
pub fn show_fatal_error(_msg: &str) {}

/// the clipboard is a shared resource — a clipboard manager or another app
/// often holds it for a few ms exactly when we want it. retry briefly (1+2+4+8
/// ms backoff) instead of silently dropping the copy or paste
#[cfg(windows)]
fn open_clipboard_retry() -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::DataExchange::OpenClipboard;
    for attempt in 0..5u32 {
        if unsafe { OpenClipboard(Some(HWND(std::ptr::null_mut()))) }.is_ok() {
            return true;
        }
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(1 << attempt));
        }
    }
    false
}

/// set the clipboard to `text` as CF_UNICODETEXT via Win32 directly (avoids a
/// clipboard crate that drags in image-decoder dependencies for text-only use)
#[cfg(windows)]
pub fn clipboard_set(text: &str) {
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    // GlobalFree isn't in the windows crate's generated bindings; declare it
    // directly (kernel32) like the SetErrorMode extern above
    unsafe extern "system" {
        fn GlobalFree(hmem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }

    // utf-16, nul-terminated
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * std::mem::size_of::<u16>();

    if !open_clipboard_retry() {
        return;
    }
    unsafe {
        // wrap the body so the clipboard is always closed, even on early return
        let res = (|| {
            EmptyClipboard().ok()?;
            // GMEM_MOVEABLE block; the system takes ownership only once
            // SetClipboardData succeeds — until then we must free it ourselves
            let hglobal: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
            let dst = GlobalLock(hglobal);
            if dst.is_null() {
                let _ = GlobalFree(hglobal.0);
                return None;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, dst as *mut u8, bytes);
            let _ = GlobalUnlock(hglobal);
            if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hglobal.0))).is_err() {
                let _ = GlobalFree(hglobal.0);
                return None;
            }
            Some(())
        })();
        let _ = res;
        let _ = CloseClipboard();
    }
}

/// read CF_UNICODETEXT from the clipboard as a String (empty if none/unavailable)
#[cfg(windows)]
pub fn clipboard_get() -> String {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let mut out = String::new();
    if !open_clipboard_retry() {
        return out;
    }
    unsafe {
        if let Ok(h) = GetClipboardData(CF_UNICODETEXT.0 as u32)
            && !h.0.is_null() {
                let hglobal = HGLOBAL(h.0);
                let ptr = GlobalLock(hglobal) as *const u16;
                if !ptr.is_null() {
                    // GlobalSize is bytes incl. the trailing nul; clamp to it
                    let cap = GlobalSize(hglobal) / std::mem::size_of::<u16>();
                    let mut len = 0usize;
                    while len < cap && *ptr.add(len) != 0 {
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(ptr, len);
                    out = String::from_utf16_lossy(slice);
                    let _ = GlobalUnlock(hglobal);
                }
            }
        let _ = CloseClipboard();
    }
    out
}

#[cfg(not(windows))]
pub fn clipboard_set(_text: &str) {}

#[cfg(not(windows))]
pub fn clipboard_get() -> String {
    String::new()
}

/// register a process-global hotkey on a dedicated thread and call `on_press`
/// each time it fires. returns true if it registered (false if the combination
/// is already taken). the thread lives for the process; the os frees the hotkey
/// on exit
#[cfg(windows)]
pub fn spawn_global_hotkey(id: i32, modifiers: u32, vk: u32, on_press: impl Fn() + Send + 'static) -> bool {
    use std::sync::mpsc;
    use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, HOT_KEY_MODIFIERS};
    use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};
    // RegisterHotKey with a null hwnd posts WM_HOTKEY to the calling thread's
    // queue, so register and pump on the same dedicated thread
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || unsafe {
        let ok = RegisterHotKey(None, id, HOT_KEY_MODIFIERS(modifiers), vk).is_ok();
        let _ = tx.send(ok);
        if !ok {
            return;
        }
        let mut msg = MSG::default();
        // GetMessageW returns >0 for a message, 0 for WM_QUIT, -1 on error
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if msg.message == WM_HOTKEY {
                on_press();
            }
        }
    });
    rx.recv().unwrap_or(false)
}

#[cfg(not(windows))]
pub fn spawn_global_hotkey(_id: i32, _modifiers: u32, _vk: u32, _on_press: impl Fn() + Send + 'static) -> bool {
    false
}

/// whether either ctrl key is physically down right now, read from the os rather
/// than tracked modifier state. winit can miss a ctrl release (after a focus
/// change or a tui that grabs input), leaving the tracked state stuck; reading
/// the real key here keeps the ctrl-hover link from latching on without ctrl
#[cfg(windows)]
pub fn ctrl_held() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL};
    unsafe { (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0 }
}

#[cfg(not(windows))]
pub fn ctrl_held() -> bool {
    false
}

/// the foreground window right now, as a raw HWND value (0 if none). captured at
/// startup to remember which window launched us, before we create our own
#[cfg(windows)]
pub fn foreground_window() -> isize {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow().0 as isize }
}

#[cfg(not(windows))]
pub fn foreground_window() -> isize {
    0
}

struct ComGuard {
    needs_uninit: bool,
}

impl ComGuard {
    fn new() -> Self {
        unsafe {
            let needs_uninit = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
            .is_ok();
            Self { needs_uninit }
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.needs_uninit {
            unsafe {
                windows::Win32::System::Com::CoUninitialize();
            }
        }
    }
}

/// if `hwnd` is an explorer file window, the folder it is showing as a `file://`
/// url; None otherwise. used to recover the launch folder when the explorer
/// address bar starts a bare `termie` with no working dir of its own. a
/// non-explorer window, a virtual folder (This PC, etc.), or any com failure
/// yields None — every step is fallible and never panics
#[cfg(windows)]
pub fn explorer_dir_for(hwnd: isize) -> Option<String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CLSCTX_ALL, CoCreateInstance,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    use windows::core::Interface;

    if hwnd == 0 {
        return None;
    }
    let h = HWND(hwnd as *mut core::ffi::c_void);
    // cheap early-out: only explorer file windows carry a folder, so we skip com
    // entirely for the start menu, taskbar, run box, desktop and every other window
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    let class = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
    if class != "CabinetWClass" && class != "ExploreWClass" {
        return None;
    }
    let _com_guard = ComGuard::new();
    unsafe {
        let shell_windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
        let count = shell_windows.Count().ok()?;
        for i in 0..count {
            let Ok(disp) = shell_windows.Item(&VARIANT::from(i)) else {
                continue;
            };
            let Ok(wb) = disp.cast::<IWebBrowser2>() else {
                continue;
            };
            let matches = wb.HWND().map(|w| w.0 == hwnd).unwrap_or(false);
            if !matches {
                continue;
            }
            let url = wb.LocationURL().ok()?.to_string();
            return (!url.is_empty()).then_some(url);
        }
    }
    None
}

#[cfg(not(windows))]
pub fn explorer_dir_for(_hwnd: isize) -> Option<String> {
    None
}
