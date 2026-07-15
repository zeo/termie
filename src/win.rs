//! the platform layer: window effects, clipboard, shell/OS integration.
//! every function has an implementation per OS — real Win32/DWM/COM on
//! windows, XDG/wayland/x11 equivalents on unix, and an honest no-op where
//! the concept doesn't exist on the other side (jump lists, UAC, WSL)

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

#[cfg(not(windows))]
fn parse_portal_color_scheme(text: &str) -> Option<bool> {
    let value = text.split("uint32").nth(1)?.trim_start().chars().next()?;
    match value {
        '1' => Some(true),
        '2' => Some(false),
        _ => None,
    }
}

#[cfg(not(windows))]
fn system_theme_is_dark() -> Option<bool> {
    let output = std::process::Command::new("gdbus")
        .args([
            "call",
            "--timeout",
            "2",
            "--session",
            "--dest",
            "org.freedesktop.portal.Desktop",
            "--object-path",
            "/org/freedesktop/portal/desktop",
            "--method",
            "org.freedesktop.portal.Settings.Read",
            "org.freedesktop.appearance",
            "color-scheme",
        ])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse_portal_color_scheme(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}

#[cfg(not(windows))]
pub fn watch_system_theme(on_change: impl Fn(Option<bool>) + Send + 'static) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        on_change(system_theme_is_dark());
        let mut command = std::process::Command::new("gdbus");
        command
            .args([
                "monitor",
                "--session",
                "--dest",
                "org.freedesktop.portal.Desktop",
                "--object-path",
                "/org/freedesktop/portal/desktop",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let Ok(mut child) = command.spawn() else {
            return;
        };
        let Some(stdout) = child.stdout.take() else {
            return;
        };
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("color-scheme") {
                on_change(system_theme_is_dark());
            }
        }
        let _ = child.wait();
    });
}

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
pub fn set_taskbar_progress(window: &winit::window::Window, state: u8, pct: u8) {
    use std::cell::OnceCell;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        ITaskbarList3, TaskbarList, TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL,
        TBPF_PAUSED,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    thread_local! {
        // created once per thread; None caches a failed creation so we don't
        // retry com on every output chunk
        static TASKBAR: OnceCell<Option<ITaskbarList3>> = const { OnceCell::new() };
    }
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
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

#[cfg(target_os = "linux")]
fn launcher_progress_properties(state: u8, pct: u8) -> String {
    let visible = matches!(state, 1 | 2 | 4);
    let urgent = state == 2;
    let updating = state == 3;
    format!(
        "{{'progress': <{:.2}>, 'progress-visible': <{visible}>, 'urgent': <{urgent}>, 'updating': <{updating}>}}",
        pct.min(100) as f64 / 100.0
    )
}

#[cfg(target_os = "linux")]
pub fn set_taskbar_progress(_window: &winit::window::Window, state: u8, pct: u8) {
    let properties = launcher_progress_properties(state, pct);
    let _ = std::process::Command::new("gdbus")
        .args([
            "emit",
            "--session",
            "--object-path",
            "/com/canonical/Unity/LauncherEntry",
            "--signal",
            "com.canonical.Unity.LauncherEntry.Update",
            "application://termie.desktop",
            &properties,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn set_taskbar_progress(_window: &winit::window::Window, _state: u8, _pct: u8) {}

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

/// the cross-platform "needs attention" signal for a bell in an unfocused
/// window: taskbar flash on windows, urgency hint / attention request elsewhere
pub fn request_attention(w: &winit::window::Window) {
    #[cfg(windows)]
    {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(handle) = w.window_handle()
            && let RawWindowHandle::Win32(h) = handle.as_raw()
        {
            flash_taskbar(h.hwnd.get());
        }
    }
    #[cfg(not(windows))]
    w.request_user_attention(Some(winit::window::UserAttentionType::Informational));
}

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

/// local wall-clock time as (year, month, day, hour, minute, second)
#[cfg(unix)]
pub fn local_ymdhms() -> (i32, u32, u32, u32, u32, u32) {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (
            tm.tm_year + 1900,
            (tm.tm_mon + 1) as u32,
            tm.tm_mday as u32,
            tm.tm_hour as u32,
            tm.tm_min as u32,
            tm.tm_sec as u32,
        )
    }
}

#[cfg(unix)]
pub fn local_hm() -> String {
    let (_, _, _, h, m, _) = local_ymdhms();
    format!("{h:02}:{m:02}")
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

/// open an http(s) URL in the default browser via xdg-open. the scheme is
/// re-checked here so only web links can ever be launched
#[cfg(unix)]
pub fn open_url(url: &str) {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return;
    }
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

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

#[cfg(target_os = "linux")]
fn terminal_list_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .and_then(|s| s.split(':').find(|part| !part.is_empty()).map(str::to_ascii_lowercase))
        .filter(|s| s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')));
    Some(base.join(match desktop {
        Some(name) => format!("{name}-xdg-terminals.list"),
        None => "xdg-terminals.list".to_string(),
    }))
}

#[cfg(target_os = "linux")]
fn terminal_list_with_termie(contents: &str, enabled: bool) -> String {
    let mut lines: Vec<&str> = contents
        .lines()
        .filter(|line| line.trim() != "termie.desktop")
        .collect();
    if enabled {
        lines.insert(0, "termie.desktop");
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

/// true when termie is the first explicit choice for this desktop
#[cfg(target_os = "linux")]
pub fn defterm_registered() -> bool {
    let Some(path) = terminal_list_path() else {
        return false;
    };
    std::fs::read_to_string(path)
        .ok()
        .is_some_and(|contents| {
            contents.lines().map(str::trim).find(|line| {
                !line.is_empty()
                    && !line.starts_with('#')
                    && !line.starts_with('+')
                    && !line.starts_with('-')
                    && line.contains(".desktop")
            }) == Some("termie.desktop")
        })
}

#[cfg(target_os = "linux")]
fn set_default_terminal(enabled: bool) -> bool {
    let Some(path) = terminal_list_path() else {
        return false;
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return false,
    };
    let updated = terminal_list_with_termie(&contents, enabled);
    if updated == contents {
        return true;
    }
    if updated.is_empty() {
        return std::fs::remove_file(path).is_ok();
    }
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let temporary = path.with_extension("list.termie-tmp");
    if std::fs::write(&temporary, updated).is_err() {
        return false;
    }
    if std::fs::rename(&temporary, path).is_ok() {
        true
    } else {
        let _ = std::fs::remove_file(temporary);
        false
    }
}

#[cfg(target_os = "linux")]
pub fn register_defterm() -> bool {
    set_default_terminal(true)
}

#[cfg(target_os = "linux")]
pub fn unregister_defterm() -> bool {
    set_default_terminal(false)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn defterm_registered() -> bool {
    false
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn register_defterm() -> bool {
    false
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn unregister_defterm() -> bool {
    false
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

/// true when running as root — the unix analogue of an elevated token
#[cfg(unix)]
pub fn is_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
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

// every defterm call site is cfg(windows) except the refresh on the startup
// worker thread, so only that one keeps a unix stub
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

/// print the fatal boot error and best-effort raise a dialog (zenity ships on
/// most gtk desktops; its absence just means stderr only)
#[cfg(unix)]
pub fn show_fatal_error(msg: &str) {
    eprintln!("termie: GPU initialization failed: {msg}");
    let _ = std::process::Command::new("zenity")
        .args(["--error", "--title", "termie \u{2014} GPU initialization failed", "--text", msg])
        .spawn();
}

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

/// linux clipboard via copypasta (smithay-clipboard on wayland, x11-clipboard
/// on x11). the provider isn't Send, and every caller is on the UI thread, so
/// it lives in a thread-local initialized once from the first window's display
#[cfg(all(unix, not(target_os = "macos")))]
mod unix_clipboard {
    use copypasta::ClipboardProvider;
    use std::cell::RefCell;

    thread_local! {
        pub(super) static CLIP: RefCell<Option<Box<dyn ClipboardProvider>>> =
            const { RefCell::new(None) };
    }

    pub(super) fn init(window: &winit::window::Window) {
        use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        let Ok(dh) = window.display_handle() else {
            return;
        };
        CLIP.with(|c| {
            let mut c = c.borrow_mut();
            if c.is_some() {
                return;
            }
            *c = match dh.as_raw() {
                RawDisplayHandle::Wayland(w) => {
                    // the wayland clipboard rides the compositor connection the
                    // window already holds; the display outlives every window
                    let (_primary, clip) = unsafe {
                        copypasta::wayland_clipboard::create_clipboards_from_external(
                            w.display.as_ptr(),
                        )
                    };
                    Some(Box::new(clip) as Box<dyn ClipboardProvider>)
                }
                _ => copypasta::x11_clipboard::X11ClipboardContext::<
                    copypasta::x11_clipboard::Clipboard,
                >::new()
                .ok()
                .map(|c| Box::new(c) as Box<dyn ClipboardProvider>),
            };
        });
    }
}

/// hook the clipboard up to the first window's display connection; a no-op on
/// windows (the Win32 clipboard needs no handle) and on repeat calls
pub fn clipboard_init(_window: &winit::window::Window) {
    #[cfg(all(unix, not(target_os = "macos")))]
    unix_clipboard::init(_window);
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn clipboard_set(text: &str) {
    unix_clipboard::CLIP.with(|c| {
        if let Some(p) = c.borrow_mut().as_mut() {
            let _ = p.set_contents(text.to_string());
        }
    });
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn clipboard_get() -> String {
    unix_clipboard::CLIP.with(|c| {
        c.borrow_mut().as_mut().and_then(|p| p.get_contents().ok()).unwrap_or_default()
    })
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

#[cfg(windows)]
struct ComGuard {
    needs_uninit: bool,
}

#[cfg(windows)]
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

#[cfg(windows)]
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

#[cfg(all(test, not(windows)))]
mod tests {
    use super::{launcher_progress_properties, parse_portal_color_scheme, terminal_list_with_termie};

    #[test]
    fn portal_color_scheme_values_map_to_dark_and_light() {
        assert_eq!(parse_portal_color_scheme("(<<uint32 1>>,)"), Some(true));
        assert_eq!(parse_portal_color_scheme("(<uint32 2>,)"), Some(false));
        assert_eq!(parse_portal_color_scheme("(<uint32 0>,)"), None);
    }

    #[test]
    fn launcher_progress_maps_terminal_states() {
        let normal = launcher_progress_properties(1, 42);
        assert!(normal.contains("'progress': <0.42>"));
        assert!(normal.contains("'progress-visible': <true>"));
        assert!(normal.contains("'urgent': <false>"));
        let error = launcher_progress_properties(2, 150);
        assert!(error.contains("'progress': <1.00>"));
        assert!(error.contains("'urgent': <true>"));
        let indeterminate = launcher_progress_properties(3, 0);
        assert!(indeterminate.contains("'progress-visible': <false>"));
        assert!(indeterminate.contains("'updating': <true>"));
        let clear = launcher_progress_properties(0, 0);
        assert!(clear.contains("'progress-visible': <false>"));
        assert!(clear.contains("'urgent': <false>"));
        assert!(clear.contains("'updating': <false>"));
    }

    #[test]
    fn default_terminal_list_preserves_the_previous_choice() {
        let original = "# preferred terminal\norg.kde.konsole.desktop\nfoot.desktop\n";
        let enabled = terminal_list_with_termie(original, true);
        assert_eq!(
            enabled,
            "termie.desktop\n# preferred terminal\norg.kde.konsole.desktop\nfoot.desktop\n"
        );
        assert_eq!(terminal_list_with_termie(&enabled, false), original);
        assert_eq!(terminal_list_with_termie("termie.desktop\n", false), "");
    }
}
