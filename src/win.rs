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

/// if `hwnd` is an explorer file window, the folder it is showing as a `file://`
/// url; None otherwise. used to recover the launch folder when the explorer
/// address bar starts a bare `termie` with no working dir of its own. a
/// non-explorer window, a virtual folder (This PC, etc.), or any com failure
/// yields None — every step is fallible and never panics
#[cfg(windows)]
pub fn explorer_dir_for(hwnd: isize) -> Option<String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
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
    unsafe {
        // winit already put this (main) thread in an STA; S_FALSE / RPC_E_CHANGED_MODE
        // are both fine to ignore, exactly like the taskbar-progress path does
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
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
