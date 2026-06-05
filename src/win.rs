//! windows-specific window effects: acrylic backdrop + rounded corners

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

/// set the clipboard to `text` as CF_UNICODETEXT via Win32 directly (avoids a
/// clipboard crate that drags in image-decoder dependencies for text-only use)
#[cfg(windows)]
pub fn clipboard_set(text: &str) {
    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
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

    unsafe {
        if OpenClipboard(Some(HWND(std::ptr::null_mut()))).is_err() {
            return;
        }
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
    use windows::Win32::Foundation::{HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let mut out = String::new();
    unsafe {
        if OpenClipboard(Some(HWND(std::ptr::null_mut()))).is_err() {
            return out;
        }
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
