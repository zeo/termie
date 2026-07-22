//! the platform layer: window effects, clipboard, shell/OS integration.
//! every function has an implementation per OS — real Win32/DWM/COM on
//! windows, XDG/wayland/x11 equivalents on unix, and an honest no-op where
//! the concept doesn't exist on the other side (UAC, WSL)

#[cfg(not(windows))]
use crate::plugin::market::{bounded_output, quiet_command};

#[cfg(not(windows))]
const MAX_DESKTOP_HELPER_OUTPUT_BYTES: usize = 64 * 1024;

#[cfg(not(windows))]
fn desktop_helper_output(command: &mut std::process::Command) -> Option<std::process::Output> {
    bounded_output(command, MAX_DESKTOP_HELPER_OUTPUT_BYTES).ok()
}

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
    let mut command = quiet_command("gdbus");
    command.args([
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
        ]);
    let output = desktop_helper_output(&mut command)?;
    output
        .status
        .success()
        .then(|| parse_portal_color_scheme(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}

#[cfg(not(windows))]
fn discard_monitor_line(reader: &mut impl std::io::BufRead) -> std::io::Result<()> {
    loop {
        let bytes = reader.fill_buf()?;
        let Some(newline) = bytes.iter().position(|&byte| byte == b'\n') else {
            let len = bytes.len();
            if len == 0 {
                return Ok(());
            }
            reader.consume(len);
            continue;
        };
        reader.consume(newline + 1);
        return Ok(());
    }
}

#[cfg(not(windows))]
fn read_monitor_line(reader: &mut impl std::io::BufRead) -> std::io::Result<Option<String>> {
    use std::io::{BufRead, Read};

    let mut line = String::new();
    let mut chunk = reader.by_ref().take(MAX_DESKTOP_HELPER_OUTPUT_BYTES as u64);
    let read = chunk.read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if read == MAX_DESKTOP_HELPER_OUTPUT_BYTES && !line.ends_with('\n') {
        discard_monitor_line(reader)?;
        line.clear();
    }
    Ok(Some(line))
}

#[cfg(not(windows))]
pub fn watch_system_theme(on_change: impl Fn(Option<bool>) + Send + 'static) {
    std::thread::spawn(move || {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        on_change(system_theme_is_dark());
        let mut command = quiet_command("gdbus");
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
        let mut reader = std::io::BufReader::new(stdout);
        while let Ok(Some(line)) = read_monitor_line(&mut reader) {
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

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq)]
pub struct KwinDragSnapshot {
    pub generation: u64,
    pub windows: Vec<(usize, f64, f64, f64, f64)>,
}

#[cfg(target_os = "linux")]
fn parse_kwin_drag_snapshot(payload: &str) -> Option<KwinDragSnapshot> {
    let mut lines = payload.lines();
    let generation = lines.next()?.parse().ok()?;
    let mut windows = Vec::new();
    for line in lines {
        if windows.len() == 256 {
            return None;
        }
        let mut fields = line.split(',');
        let row = (
            fields.next()?.parse().ok()?,
            fields.next()?.parse().ok()?,
            fields.next()?.parse().ok()?,
            fields.next()?.parse().ok()?,
            fields.next()?.parse().ok()?,
        );
        if fields.next().is_some()
            || ![row.1, row.2, row.3, row.4].into_iter().all(f64::is_finite)
            || row.3 <= 0.0
            || row.4 <= 0.0
        {
            return None;
        }
        windows.push(row);
    }
    Some(KwinDragSnapshot { generation, windows })
}

#[cfg(target_os = "linux")]
struct KwinDragSink {
    proxy: winit::event_loop::EventLoopProxy<crate::UserEvent>,
}

#[cfg(target_os = "linux")]
#[zbus::interface(name = "org.termie.Drag")]
impl KwinDragSink {
    fn geometry(&self, payload: &str) -> bool {
        parse_kwin_drag_snapshot(payload)
            .is_some_and(|snapshot| self.proxy.send_event(crate::UserEvent::KwinDragGeometry(snapshot)).is_ok())
    }
}

#[cfg(target_os = "linux")]
pub struct KwinDragBridge {
    _connection: zbus::blocking::Connection,
    service: String,
}

#[cfg(target_os = "linux")]
impl KwinDragBridge {
    pub fn new(proxy: winit::event_loop::EventLoopProxy<crate::UserEvent>) -> Option<Self> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() || !is_kde_desktop() {
            return None;
        }
        let service = format!("org.termie.Drag.p{}", std::process::id());
        let connection = zbus::blocking::connection::Builder::session()
            .ok()?
            .name(service.as_str())
            .ok()?
            .serve_at("/org/termie/Drag", KwinDragSink { proxy })
            .ok()?
            .build()
            .ok()?;
        Some(Self { _connection: connection, service })
    }

    pub fn request(&self, generation: u64) -> Option<String> {
        let pid = std::process::id();
        let name = format!("termie-drag-geometry-{pid}-{generation}");
        let script = kwin_drag_script(&self.service, pid, generation);
        let (id, path) = load_kwin_script(&name, &script)?;
        let ran = run_kwin_script(id);
        let _ = std::fs::remove_file(path);
        if ran {
            Some(name)
        } else {
            unload_kwin_script(&name);
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn kwin_drag_script(service: &str, pid: u32, generation: u64) -> String {
    format!(
        "const marker = '\\u2063\\u2063\\u2063';\nconst bit = '\\u200b';\nlet payload = '{generation}';\nconst windows = workspace.stackingOrder;\nfor (let i = 0; i < windows.length; i++) {{\n    const window = windows[i];\n    if (window.pid !== {pid}) continue;\n    const caption = window.caption;\n    const markerAt = caption.indexOf(marker);\n    if (markerAt < 0) continue;\n    let tagged = markerAt + marker.length;\n    let index = 0;\n    while (caption.charAt(tagged + index) === bit) index++;\n    if (index === 0) continue;\n    const geometry = window.clientGeometry;\n    payload += '\\n' + (index - 1) + ',' + geometry.x + ',' + geometry.y + ',' + geometry.width + ',' + geometry.height;\n}}\ncallDBus('{service}', '/org/termie/Drag', 'org.termie.Drag', 'Geometry', payload);\n"
    )
}

#[cfg(target_os = "linux")]
fn is_kde_desktop() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .split(':')
        .any(|part| matches!(part, "kde" | "plasma"))
}

#[cfg(target_os = "linux")]
fn kwin_keep_above_script(pid: u32, width: f64, height: f64, on: bool) -> String {
    kwin_window_script(pid, width, height, &format!("    target.keepAbove = {on};\n"))
}

#[cfg(target_os = "linux")]
fn kwin_quake_script(pid: u32, width: f64, height: f64) -> String {
    kwin_window_script(
        pid,
        width,
        height,
        "    const area = workspace.clientArea(KWin.MaximizeArea, target);\n    target.minimized = false;\n    target.frameGeometry = { x: area.x, y: area.y, width: area.width, height: Math.max(120, Math.round(area.height * 0.45)) };\n    target.keepAbove = true;\n    workspace.activeWindow = target;\n",
    )
}

#[cfg(target_os = "linux")]
fn kwin_hide_quake_script(pid: u32, width: f64, height: f64) -> String {
    kwin_window_script(pid, width, height, "    target.minimized = true;\n")
}

#[cfg(target_os = "linux")]
fn kwin_window_script(pid: u32, width: f64, height: f64, action: &str) -> String {
    format!(
        "function matchesTarget(window) {{\n    return window.pid === {pid}\n        && Math.abs(window.width - {width:.3}) < 2\n        && Math.abs(window.height - {height:.3}) < 2;\n}}\nlet target = workspace.activeWindow;\nif (!target || !matchesTarget(target)) {{\n    const windows = workspace.windowList();\n    let match = null;\n    let matches = 0;\n    for (let i = 0; i < windows.length; i++) {{\n        if (matchesTarget(windows[i])) {{\n            match = windows[i];\n            matches++;\n        }}\n    }}\n    if (matches === 1) target = match;\n}}\nif (target && matchesTarget(target)) {{\n{action}}}\n"
    )
}

/// apply keep-above to the active termie window on kde wayland, where winit's
/// generic window-level call is currently unsupported
#[cfg(target_os = "linux")]
pub fn set_window_above(window: &winit::window::Window, on: bool) {
    let _ = apply_kwin_window_script(window, "keep-above", |pid, width, height| {
        kwin_keep_above_script(pid, width, height, on)
    });
}

#[cfg(target_os = "linux")]
pub fn show_quake_window(window: &winit::window::Window) -> bool {
    apply_kwin_window_script(window, "quake", kwin_quake_script)
}

#[cfg(target_os = "linux")]
pub fn hide_quake_window(window: &winit::window::Window) -> bool {
    apply_kwin_window_script(window, "hide-quake", kwin_hide_quake_script)
}

#[cfg(target_os = "linux")]
fn apply_kwin_window_script(
    window: &winit::window::Window,
    purpose: &str,
    script: impl FnOnce(u32, f64, f64) -> String,
) -> bool {
    if !is_kde_desktop() {
        return false;
    }
    let pid = std::process::id();
    let name = format!("termie-{purpose}-{pid}");
    let size = window.inner_size();
    let scale = window.scale_factor();
    let script = script(pid, size.width as f64 / scale, size.height as f64 / scale);
    let Some((id, path)) = load_kwin_script(&name, &script) else {
        return false;
    };
    let handled = run_kwin_script(id);
    let _ = std::fs::remove_file(path);
    unload_kwin_script(&name);
    handled
}

#[cfg(target_os = "linux")]
fn load_kwin_script(name: &str, script: &str) -> Option<(u32, std::path::PathBuf)> {
    let dir = crate::cache_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{name}.js"));
    std::fs::write(&path, script).ok()?;
    let mut command = quiet_command("gdbus");
    command.args([
            "call",
            "--session",
            "--dest",
            "org.kde.KWin",
            "--object-path",
            "/Scripting",
            "--method",
            "org.kde.kwin.Scripting.loadScript",
        ])
        .arg(&path)
        .arg(name);
    let id = desktop_helper_output(&mut command).and_then(|output| {
        let id = output.status.success().then(|| String::from_utf8_lossy(&output.stdout)).and_then(|text| {
            text.split(|c: char| !c.is_ascii_digit() && c != '-')
                .filter_map(|part| part.parse::<i32>().ok())
                .next_back()
                .and_then(|id| u32::try_from(id).ok())
        });
        if id.is_none() {
            log::warn!(
                "KWin rejected {name}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        id
    });
    match id {
        Some(id) => Some((id, path)),
        None => {
            let _ = std::fs::remove_file(path);
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn run_kwin_script(id: u32) -> bool {
    std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.kde.KWin",
            "--object-path",
            &format!("/Scripting/Script{id}"),
            "--method",
            "org.kde.kwin.Script.run",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "linux")]
pub fn unload_kwin_script(name: &str) {
    let _ = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.kde.KWin",
            "--object-path",
            "/Scripting",
            "--method",
            "org.kde.kwin.Scripting.unloadScript",
            name,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(target_os = "linux"))]
pub fn set_window_above(_window: &winit::window::Window, _on: bool) {}

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
fn emit_launcher_update(properties: &str) {
    let _ = std::process::Command::new("gdbus")
        .args([
            "emit",
            "--session",
            "--object-path",
            "/com/canonical/Unity/LauncherEntry",
            "--signal",
            "com.canonical.Unity.LauncherEntry.Update",
            "application://termie.desktop",
            properties,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(target_os = "linux")]
pub fn set_taskbar_progress(_window: &winit::window::Window, state: u8, pct: u8) {
    emit_launcher_update(&launcher_progress_properties(state, pct));
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

#[cfg(target_os = "linux")]
fn desktop_exec_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        match c {
            '"' | '`' | '$' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '\\' => quoted.push_str(r"\\\\"),
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(target_os = "linux")]
fn desktop_name_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => {},
            _ => escaped.push(c),
        }
    }
    escaped
}

#[cfg(target_os = "linux")]
fn desktop_with_profiles(contents: &str, tasks: &[(String, String)], exe: &str) -> String {
    let profiles: Vec<_> = tasks
        .iter()
        .filter(|(_, args)| {
            !matches!(
                args.as_str(),
                "" | "--shell bash" | "--shell zsh" | "--shell fish"
            )
        })
        .collect();
    let ids: Vec<String> = (0..profiles.len()).map(|i| format!("TermieProfile{i}")).collect();
    let mut kept = Vec::new();
    let mut generated_section = false;
    let mut actions_seen = false;
    for line in contents.lines() {
        if line.starts_with('[') {
            generated_section = line.starts_with("[Desktop Action TermieProfile");
        }
        if generated_section {
            continue;
        }
        if let Some(actions) = line.strip_prefix("Actions=") {
            let mut actions: Vec<&str> = actions
                .split(';')
                .filter(|action| !action.is_empty() && !action.starts_with("TermieProfile"))
                .collect();
            actions.extend(ids.iter().map(String::as_str));
            kept.push(format!("Actions={};", actions.join(";")));
            actions_seen = true;
        } else {
            kept.push(line.to_string());
        }
    }
    if !actions_seen || profiles.is_empty() {
        return if contents.ends_with('\n') {
            format!("{}\n", kept.join("\n"))
        } else {
            kept.join("\n")
        };
    }
    let exe = desktop_exec_quote(exe);
    for ((title, _), id) in profiles.iter().zip(&ids) {
        if !kept.last().is_some_and(String::is_empty) {
            kept.push(String::new());
        }
        let profile = title.strip_prefix("new window: ").unwrap_or(title);
        kept.push(format!("[Desktop Action {id}]"));
        kept.push(format!("Name={}", desktop_name_escape(title)));
        kept.push(format!("Exec={exe} --shell {}", desktop_exec_quote(profile)));
    }
    format!("{}\n", kept.join("\n"))
}

#[cfg(target_os = "linux")]
fn installed_desktop_path(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let beside = exe.parent()?.parent()?.join("share/applications/termie.desktop");
    if beside.is_file() {
        return Some(beside);
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .map(|home| home.join(".local/share"))
        })?;
    let desktop = data.join("applications/termie.desktop");
    desktop.is_file().then_some(desktop)
}

#[cfg(target_os = "linux")]
pub fn update_jumplist(tasks: &[(String, String)]) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(desktop) = installed_desktop_path(&exe) else {
        return;
    };
    let Ok(Some(contents)) = read_integration_text(&desktop) else {
        return;
    };
    let updated = desktop_with_profiles(&contents, tasks, &exe.to_string_lossy());
    if updated == contents {
        return;
    }
    let temporary = desktop.with_extension("desktop.termie-tmp");
    if std::fs::write(&temporary, updated).is_err() {
        return;
    }
    if std::fs::rename(&temporary, &desktop).is_err() {
        let _ = std::fs::remove_file(temporary);
        return;
    }
    if let Some(dir) = desktop.parent() {
        let _ = std::process::Command::new("update-desktop-database")
            .arg(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
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
    #[cfg(target_os = "linux")]
    emit_launcher_update("{'urgent': <true>}");
}

pub fn clear_attention(w: &winit::window::Window) {
    w.request_user_attention(None);
    #[cfg(target_os = "linux")]
    emit_launcher_update("{'urgent': <false>}");
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

fn web_url_is_safe(url: &str) -> bool {
    let rest = if url.get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://")) {
        &url[7..]
    } else if url.get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://")) {
        &url[8..]
    } else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    !host.is_empty()
        && !host.starts_with(':')
        && !url.chars().any(|c| c.is_whitespace() || c.is_control() || c == '\\')
}

/// open an http(s) URL in the default browser via the shell. the scheme is
/// re-checked here so only web links can ever be launched, never a file path
/// or a custom protocol handler that could start an arbitrary app
#[cfg(windows)]
pub fn open_url(url: &str) -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;
    if !web_url_is_safe(url) {
        return false;
    }
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    result.0 as isize > 32
}

/// open an http(s) URL in the default browser via xdg-open. the scheme is
/// re-checked here so only web links can ever be launched
#[cfg(unix)]
pub fn open_url(url: &str) -> bool {
    if !web_url_is_safe(url) {
        return false;
    }
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
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
const MAX_LINUX_INTEGRATION_BYTES: usize = 1024 * 1024;

#[cfg(target_os = "linux")]
fn read_limited(reader: impl std::io::Read, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;

    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() <= limit).then_some(bytes))
}

#[cfg(target_os = "linux")]
fn read_integration_text(path: &std::path::Path) -> std::io::Result<Option<String>> {
    let file = std::fs::File::open(path)?;
    Ok(read_limited(file, MAX_LINUX_INTEGRATION_BYTES)?.and_then(|bytes| String::from_utf8(bytes).ok()))
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

#[cfg(target_os = "linux")]
fn xdg_defterm_registered() -> bool {
    let Some(path) = terminal_list_path() else {
        return false;
    };
    read_integration_text(&path)
        .ok()
        .flatten()
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
fn set_xdg_default_terminal(enabled: bool) -> bool {
    let Some(path) = terminal_list_path() else {
        return false;
    };
    let contents = match read_integration_text(&path) {
        Ok(Some(contents)) => contents,
        Ok(None) => return false,
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
fn kde_desktop() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .any(|part| part.eq_ignore_ascii_case("kde") || part.eq_ignore_ascii_case("plasma"))
}

#[cfg(target_os = "linux")]
fn kde_terminal_value(key: &str) -> Option<String> {
    const MISSING: &str = "termie-kconfig-missing-7a19f20d";
    let mut command = quiet_command("kreadconfig6");
    command.args([
            "--file",
            "kdeglobals",
            "--group",
            "General",
            "--key",
            key,
            "--default",
            MISSING,
        ]);
    let output = desktop_helper_output(&mut command)?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim_end_matches(['\r', '\n']);
    (value != MISSING).then(|| value.to_string())
}

#[cfg(target_os = "linux")]
fn write_kde_terminal_value(key: &str, value: Option<&str>) -> bool {
    let mut command = std::process::Command::new("kwriteconfig6");
    command.args([
        "--file",
        "kdeglobals",
        "--group",
        "General",
        "--key",
        key,
        "--notify",
    ]);
    match value {
        Some(value) => {
            command.arg(value);
        }
        None => {
            command.args(["--delete", ""]);
        }
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "linux")]
fn kde_terminal_snapshot(
    application: Option<&str>,
    service: Option<&str>,
) -> Vec<u8> {
    let mut snapshot = Vec::new();
    for value in [application, service] {
        snapshot.push(if value.is_some() { b'1' } else { b'0' });
        if let Some(value) = value {
            snapshot.extend_from_slice(value.as_bytes());
        }
        snapshot.push(0);
    }
    snapshot
}

#[cfg(target_os = "linux")]
fn parse_kde_terminal_snapshot(snapshot: &[u8]) -> Option<(Option<String>, Option<String>)> {
    let mut fields = snapshot.split(|byte| *byte == 0);
    let mut next = || {
        let field = fields.next()?;
        match field.split_first()? {
            (b'0', _) => Some(None),
            (b'1', value) => Some(Some(String::from_utf8(value.to_vec()).ok()?)),
            _ => None,
        }
    };
    Some((next()?, next()?))
}

#[cfg(target_os = "linux")]
fn kde_terminal_snapshot_path() -> Option<std::path::PathBuf> {
    Some(crate::app_dir()?.join("default-terminal-kde"))
}

#[cfg(target_os = "linux")]
fn save_kde_terminal_snapshot(application: Option<&str>, service: Option<&str>) -> bool {
    let Some(path) = kde_terminal_snapshot_path() else {
        return false;
    };
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let temporary = path.with_extension("termie-tmp");
    if std::fs::write(&temporary, kde_terminal_snapshot(application, service)).is_err() {
        return false;
    }
    if std::fs::rename(&temporary, &path).is_ok() {
        true
    } else {
        let _ = std::fs::remove_file(temporary);
        false
    }
}

#[cfg(target_os = "linux")]
fn restore_kde_default_terminal() -> bool {
    let path = kde_terminal_snapshot_path();
    let previous = match path.as_ref().map(|path| {
        let file = std::fs::File::open(path)?;
        read_limited(file, MAX_LINUX_INTEGRATION_BYTES)
    }) {
        Some(Ok(Some(snapshot))) => {
            let Some(previous) = parse_kde_terminal_snapshot(&snapshot) else {
                return false;
            };
            previous
        }
        Some(Ok(None)) => return false,
        Some(Err(error)) if error.kind() != std::io::ErrorKind::NotFound => return false,
        _ => (None, None),
    };
    let application_restored =
        write_kde_terminal_value("TerminalApplication", previous.0.as_deref());
    let service_restored = write_kde_terminal_value("TerminalService", previous.1.as_deref());
    let restored = application_restored && service_restored;
    if restored && let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
    restored
}

#[cfg(target_os = "linux")]
fn command_quote(text: &str) -> String {
    if text.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-')
    }) {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(target_os = "linux")]
fn set_kde_default_terminal() -> bool {
    let application = kde_terminal_value("TerminalApplication");
    let service = kde_terminal_value("TerminalService");
    if service.as_deref() == Some("termie.desktop") {
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    if !save_kde_terminal_snapshot(application.as_deref(), service.as_deref()) {
        return false;
    }
    let executable = command_quote(&exe.to_string_lossy());
    if write_kde_terminal_value("TerminalApplication", Some(&executable))
        && write_kde_terminal_value("TerminalService", Some("termie.desktop"))
    {
        true
    } else {
        let _ = restore_kde_default_terminal();
        false
    }
}

/// true when termie is the desktop's explicit terminal choice
#[cfg(target_os = "linux")]
pub fn defterm_registered() -> bool {
    if kde_desktop() {
        kde_terminal_value("TerminalService").as_deref() == Some("termie.desktop")
    } else {
        xdg_defterm_registered()
    }
}

#[cfg(target_os = "linux")]
pub fn register_defterm() -> bool {
    if kde_desktop() {
        let registered = set_kde_default_terminal();
        if registered {
            let _ = set_xdg_default_terminal(true);
        }
        registered
    } else {
        set_xdg_default_terminal(true)
    }
}

#[cfg(target_os = "linux")]
pub fn unregister_defterm() -> bool {
    if kde_desktop() {
        let restored = restore_kde_default_terminal();
        if restored {
            let _ = set_xdg_default_terminal(false);
        }
        restored
    } else {
        set_xdg_default_terminal(false)
    }
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

#[cfg(target_os = "linux")]
pub fn refresh_defterm_server_path() {
    if !kde_desktop()
        || kde_terminal_value("TerminalService").as_deref() != Some("termie.desktop")
    {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if exe.ancestors().any(|path| {
        path.file_name().is_some_and(|name| name.eq_ignore_ascii_case("target"))
            || path.join("Cargo.toml").is_file()
    }) {
        return;
    }
    let executable = command_quote(&exe.to_string_lossy());
    let _ = write_kde_terminal_value("TerminalApplication", Some(&executable));
}

#[cfg(not(any(windows, target_os = "linux")))]
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

pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum ClipboardRead {
    Text(String),
    TooLarge,
}

fn bounded_clipboard_text(text: String) -> ClipboardRead {
    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        ClipboardRead::TooLarge
    } else {
        ClipboardRead::Text(text)
    }
}

/// read CF_UNICODETEXT from the clipboard as a String (empty if none/unavailable)
#[cfg(windows)]
pub fn clipboard_get() -> ClipboardRead {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let mut out = ClipboardRead::Text(String::new());
    if !open_clipboard_retry() {
        return out;
    }
    unsafe {
        if let Ok(h) = GetClipboardData(CF_UNICODETEXT.0 as u32)
            && !h.0.is_null() {
                let hglobal = HGLOBAL(h.0);
                let ptr = GlobalLock(hglobal) as *const u16;
                if !ptr.is_null() {
                    let cap = GlobalSize(hglobal) / std::mem::size_of::<u16>();
                    let mut len = 0usize;
                    let limit = cap.min(MAX_CLIPBOARD_TEXT_BYTES / 3 + 1);
                    while len < limit && *ptr.add(len) != 0 {
                        len += 1;
                    }
                    if len == MAX_CLIPBOARD_TEXT_BYTES / 3 + 1 {
                        out = ClipboardRead::TooLarge;
                    } else {
                        let slice = std::slice::from_raw_parts(ptr, len);
                        out = bounded_clipboard_text(String::from_utf16_lossy(slice));
                    }
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

/// drop the wayland clipboard while winit's display connection is still alive
pub fn clipboard_shutdown() {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let clipboard = unix_clipboard::CLIP.with(|clipboard| clipboard.borrow_mut().take());
        drop(clipboard);
    }
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
pub fn clipboard_get() -> ClipboardRead {
    unix_clipboard::CLIP.with(|c| {
        bounded_clipboard_text(
            c.borrow_mut().as_mut().and_then(|p| p.get_contents().ok()).unwrap_or_default(),
        )
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

#[cfg(target_os = "linux")]
pub fn spawn_global_hotkey(trigger: String, on_press: impl Fn() + Send + 'static) -> bool {
    std::thread::Builder::new()
        .name("termie-quake-hotkey".to_string())
        .spawn(move || {
            if let Err(error) = run_global_shortcut_portal(&trigger, on_press) {
                log::warn!("quake hotkey unavailable: {error}");
            }
        })
        .is_ok()
}

#[cfg(target_os = "linux")]
fn run_global_shortcut_portal(
    trigger: &str,
    on_press: impl Fn() + Send + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Str};

    const DESTINATION: &str = "org.freedesktop.portal.Desktop";
    const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
    const SHORTCUTS_INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
    static TOKEN: AtomicU64 = AtomicU64::new(1);

    fn token(prefix: &str, counter: &AtomicU64) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        format!(
            "termie_{prefix}_{}_{}_{nanos}",
            std::process::id(),
            counter.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn option(value: &str) -> OwnedValue {
        OwnedValue::from(Str::from(value))
    }

    fn request_path(connection: &Connection, token: &str) -> Result<OwnedObjectPath, Box<dyn std::error::Error>> {
        let sender = connection
            .unique_name()
            .ok_or("session bus did not assign a unique name")?
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        Ok(OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{sender}/{token}"
        ))?)
    }

    fn response(
        connection: &Connection,
        expected: &OwnedObjectPath,
        returned: &OwnedObjectPath,
        mut expected_signals: zbus::blocking::proxy::SignalIterator<'_>,
    ) -> Result<HashMap<String, OwnedValue>, Box<dyn std::error::Error>> {
        let message = if expected == returned {
            expected_signals.next().ok_or("portal request ended without a response")?
        } else {
            drop(expected_signals);
            let request = Proxy::new(
                connection,
                DESTINATION,
                returned.as_str(),
                "org.freedesktop.portal.Request",
            )?;
            request
                .receive_signal("Response")?
                .next()
                .ok_or("portal request ended without a response")?
        };
        let (code, values): (u32, HashMap<String, OwnedValue>) = message.body().deserialize()?;
        if code != 0 {
            return Err(format!("portal request was declined with response {code}").into());
        }
        Ok(values)
    }

    let connection = Connection::session()?;
    let portal = Proxy::new(&connection, DESTINATION, PORTAL_PATH, SHORTCUTS_INTERFACE)?;
    let mut activations = portal.receive_signal_with_args("Activated", &[(1, "termie-quake")])?;

    let create_token = token("create", &TOKEN);
    let session_token = token("session", &TOKEN);
    let create_path = request_path(&connection, &create_token)?;
    let create_request = Proxy::new(
        &connection,
        DESTINATION,
        create_path.as_str(),
        "org.freedesktop.portal.Request",
    )?;
    let create_signals = create_request.receive_signal("Response")?;
    let create_options = HashMap::from([
        ("handle_token", option(&create_token)),
        ("session_handle_token", option(&session_token)),
    ]);
    let returned: OwnedObjectPath = portal.call("CreateSession", &(create_options,))?;
    let mut values = response(&connection, &create_path, &returned, create_signals)?;
    let session = String::try_from(values.remove("session_handle").ok_or("portal omitted the session handle")?)?;
    let session_path = OwnedObjectPath::try_from(session)?;

    let bind_token = token("bind", &TOKEN);
    let bind_path = request_path(&connection, &bind_token)?;
    let bind_request = Proxy::new(
        &connection,
        DESTINATION,
        bind_path.as_str(),
        "org.freedesktop.portal.Request",
    )?;
    let bind_signals = bind_request.receive_signal("Response")?;
    let shortcut = HashMap::from([
        ("description", option("Show or hide the Termie drop-down")),
        ("preferred_trigger", option(trigger)),
    ]);
    let shortcuts = vec![("termie-quake", shortcut)];
    let bind_options = HashMap::from([("handle_token", option(&bind_token))]);
    let returned: OwnedObjectPath = portal.call(
        "BindShortcuts",
        &(&session_path, shortcuts, "", bind_options),
    )?;
    let mut values = response(&connection, &bind_path, &returned, bind_signals)?;
    let bound = values
        .remove("shortcuts")
        .and_then(|value| Vec::<(String, HashMap<String, OwnedValue>)>::try_from(value).ok())
        .is_some_and(|shortcuts| shortcuts.iter().any(|(id, _)| id == "termie-quake"));
    if !bound {
        return Err("portal did not bind the quake shortcut".into());
    }

    for message in &mut activations {
        let (_, shortcut, _, _): (OwnedObjectPath, String, u64, HashMap<String, OwnedValue>) =
            message.body().deserialize()?;
        if shortcut == "termie-quake" {
            on_press();
        }
    }
    Ok(())
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
    use super::{
        bounded_clipboard_text, command_quote, desktop_with_profiles, kde_terminal_snapshot,
        kwin_hide_quake_script, kwin_drag_script, kwin_keep_above_script, kwin_quake_script,
        launcher_progress_properties, parse_kde_terminal_snapshot, parse_kwin_drag_snapshot,
        parse_portal_color_scheme, read_limited, read_monitor_line, terminal_list_with_termie,
        web_url_is_safe, open_url, ClipboardRead, MAX_CLIPBOARD_TEXT_BYTES, MAX_DESKTOP_HELPER_OUTPUT_BYTES,
    };

    #[test]
    fn portal_color_scheme_values_map_to_dark_and_light() {
        assert_eq!(parse_portal_color_scheme("(<<uint32 1>>,)"), Some(true));
        assert_eq!(parse_portal_color_scheme("(<uint32 2>,)"), Some(false));
        assert_eq!(parse_portal_color_scheme("(<uint32 0>,)"), None);
    }

    #[test]
    fn integration_reader_rejects_the_first_byte_over_limit() {
        assert_eq!(read_limited(std::io::Cursor::new(b"abc"), 3).expect("read"), Some(b"abc".to_vec()));
        assert_eq!(read_limited(std::io::Cursor::new(b"abcd"), 3).expect("read"), None);
    }

    #[test]
    fn web_url_gate_allows_normal_web_urls_only() {
        assert!(web_url_is_safe("https://example.com/path?q=one#part"));
        assert!(web_url_is_safe("HTTP://EXAMPLE.COM"));
        assert!(!web_url_is_safe("file:///tmp/nope"));
        assert!(!web_url_is_safe("https:///missing-host"));
        assert!(!web_url_is_safe("https://user@"));
        assert!(!web_url_is_safe("https://:443"));
        assert!(!web_url_is_safe("https://example.com\nfile:///tmp/nope"));
        assert!(!web_url_is_safe("https://example.com/a b"));
        assert!(!web_url_is_safe("https:\\\\example.com"));
        assert!(!open_url("file:///tmp/nope"));
    }

    #[test]
    fn clipboard_text_limit_rejects_the_first_byte_over_limit() {
        let accepted = bounded_clipboard_text("x".repeat(MAX_CLIPBOARD_TEXT_BYTES));
        assert!(matches!(accepted, ClipboardRead::Text(_)));
        let rejected = bounded_clipboard_text("x".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1));
        assert_eq!(rejected, ClipboardRead::TooLarge);
    }

    #[test]
    fn theme_monitor_discards_oversized_events() {
        let mut bytes = vec![b'x'; MAX_DESKTOP_HELPER_OUTPUT_BYTES + 1];
        bytes.push(b'\n');
        bytes.extend_from_slice(b"color-scheme changed\n");
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(bytes));
        assert_eq!(read_monitor_line(&mut reader).expect("read"), Some(String::new()));
        let next = read_monitor_line(&mut reader).expect("read").expect("next line");
        assert!(next.contains("color-scheme"));
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

    #[test]
    fn kde_default_terminal_snapshot_round_trips_missing_and_quoted_values() {
        let snapshot = kde_terminal_snapshot(Some("windows7-cmd --profile 'work'"), None);
        assert_eq!(
            parse_kde_terminal_snapshot(&snapshot),
            Some((Some("windows7-cmd --profile 'work'".to_string()), None))
        );
        assert_eq!(parse_kde_terminal_snapshot(b"broken"), None);
        assert_eq!(command_quote("/home/me/.local/bin/termie"), "/home/me/.local/bin/termie");
        assert_eq!(
            command_quote("/home/me/Termie's tools/termie"),
            "'/home/me/Termie'\\''s tools/termie'"
        );
    }

    #[test]
    fn linux_launcher_profiles_replace_the_generated_actions() {
        let desktop = "[Desktop Entry]\nActions=NewWindow;NewBash;TermieProfile0;\n\n\
[Desktop Action NewWindow]\nName=New Window\nExec=termie\n\n\
[Desktop Action TermieProfile0]\nName=old\nExec=old\n";
        let tasks = vec![
            ("new window".to_string(), String::new()),
            ("new window: bash".to_string(), "--shell bash".to_string()),
            ("new window: zsh".to_string(), "--shell zsh".to_string()),
            ("new window: fish".to_string(), "--shell fish".to_string()),
            ("new window: dev tools".to_string(), "--shell \"dev tools\"".to_string()),
            ("new window: cash$box".to_string(), "--shell \"cash$box\"".to_string()),
            ("new window: 50%\\work".to_string(), "--shell \"50%\\work\"".to_string()),
        ];
        let updated = desktop_with_profiles(desktop, &tasks, "/opt/Termie $/bin/termie");
        assert!(updated.contains(
            "Actions=NewWindow;NewBash;TermieProfile0;TermieProfile1;TermieProfile2;"
        ));
        assert!(updated.contains("Name=new window: dev tools"));
        assert!(updated.contains("Exec=\"/opt/Termie \\$/bin/termie\" --shell \"dev tools\""));
        assert!(updated.contains("Exec=\"/opt/Termie \\$/bin/termie\" --shell \"cash\\$box\""));
        assert!(updated.contains("--shell \"50%%\\\\\\\\work\""));
        assert!(!updated.contains("Name=old"));

        let builtins = &tasks[..4];
        let cleared = desktop_with_profiles(&updated, builtins, "/opt/termie");
        assert!(cleared.contains("Actions=NewWindow;NewBash;"));
        assert!(!cleared.contains("TermieProfile"));
    }

    #[test]
    fn kwin_keep_above_targets_only_this_process() {
        let on = kwin_keep_above_script(4217, 1000.0, 640.0, true);
        assert!(on.contains("window.pid === 4217"));
        assert!(on.contains("window.width - 1000.000"));
        assert!(on.contains("window.height - 640.000"));
        assert!(on.contains("target.keepAbove = true"));
        assert!(on.contains("workspace.activeWindow"));
        assert!(on.contains("workspace.windowList()"));
        assert!(on.contains("matches === 1"));
        let off = kwin_keep_above_script(4217, 1000.0, 640.0, false);
        assert!(off.contains("target.keepAbove = false"));
    }

    #[test]
    fn kwin_quake_uses_the_active_work_area() {
        let script = kwin_quake_script(42, 1000.0, 640.0);
        assert!(script.contains("workspace.clientArea(KWin.MaximizeArea, target)"));
        assert!(script.contains("height: Math.max(120, Math.round(area.height * 0.45))"));
        assert!(script.contains("target.keepAbove = true"));
        assert!(script.contains("workspace.activeWindow = target"));
        assert!(script.contains("target.minimized = false"));
        assert!(kwin_hide_quake_script(42, 1000.0, 640.0).contains("target.minimized = true"));
    }

    #[test]
    fn kwin_drag_geometry_script_and_payload_are_bounded() {
        let script = kwin_drag_script("org.termie.Drag.p42", 42, 7);
        assert!(script.contains("window.pid !== 42"));
        assert!(script.contains("workspace.stackingOrder"));
        assert!(script.contains("window.clientGeometry"));
        assert!(script.contains("org.termie.Drag.p42"));

        let snapshot = parse_kwin_drag_snapshot("7\n0,100,200,800,600\n1,-40,25,700.5,500")
            .expect("valid geometry snapshot");
        assert_eq!(snapshot.generation, 7);
        assert_eq!(snapshot.windows.len(), 2);
        assert!(parse_kwin_drag_snapshot("7\n0,0,0,0,100").is_none());
        assert!(parse_kwin_drag_snapshot("7\n0,NaN,0,100,100").is_none());
        assert!(parse_kwin_drag_snapshot("7\n0,0,0,100,100,extra").is_none());
    }
}
