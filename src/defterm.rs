//! windows "default terminal" handoff. when termie is registered as the
//! default terminal (HKCU\Console\%%Startup), a console app launched without a
//! terminal — from the start menu, the run box, explorer — delegates through
//! the OS console host to termie: COM starts `termie.exe -Embedding`, calls
//! ITerminalHandoff3::EstablishPtyHandoff on it with the live ConPTY pipes, and
//! the session opens as the first tab of a new termie window. the interface
//! contract is microsoft/terminal's ITerminalHandoff.idl; its proxy/stub is
//! registered system-wide by the inbox Windows Terminal, so only the vtable
//! needs implementing here.
// the interface method keeps the IDL's name and arity — this file mirrors an
// external ABI, not house style
#![allow(non_snake_case, clippy::too_many_arguments)]

use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::Arc;
use std::time::Duration;

use windows::Win32::Foundation::{E_FAIL, HANDLE, S_OK};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, CoInitializeEx, CoInitializeSecurity, CoRegisterClassObject,
    CoRevokeClassObject, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED,
    EOAC_NONE, IClassFactory, IClassFactory_Impl, REGCLS_MULTIPLEUSE, REGCLS_SINGLEUSE,
    RPC_C_AUTHN_LEVEL_NONE, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::core::{
    GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, Ref, implement, interface,
};

/// termie's terminal-handoff COM class (what DelegationTerminal points at)
pub const CLSID_TERMIE_HANDOFF: GUID = GUID::from_u128(0xd6f7e8a1_3c52_4b0f_9e6a_71b2c0a4f3d9);

/// the delegation console paired with a third-party terminal: the up-to-date
/// ConPTY host (OpenConsole) shipped inside the Windows Terminal package, inbox
/// on Windows 11. it reads DelegationTerminal and calls EstablishPtyHandoff
pub const CLSID_OPENCONSOLE: &str = "{2EACA947-7F5F-4CFA-BA87-8F7FBEEFBE69}";

/// mirrors TERMINAL_STARTUP_INFO from ITerminalHandoff.idl. the BSTR fields are
/// kept as raw pointers: the RPC stub owns them, this side only reads
#[repr(C)]
#[allow(non_snake_case)]
pub struct TerminalStartupInfo {
    pszTitle: *const u16,
    pszIconPath: *const u16,
    iconIndex: i32,
    dwX: u32,
    dwY: u32,
    dwXSize: u32,
    dwYSize: u32,
    dwXCountChars: u32,
    dwYCountChars: u32,
    dwFillAttribute: u32,
    dwFlags: u32,
    wShowWindow: u16,
}

/// ITerminalHandoff3 from microsoft/terminal src/host/proxy/ITerminalHandoff.idl.
/// `input`/`output` are [out]: the terminal creates the pipes and returns the
/// console host's ends; the rest arrive [in], already duplicated into this
/// process by the marshaler
#[interface("6F23DA90-15C5-4203-9DB0-64E73F1B1B00")]
unsafe trait ITerminalHandoff3: IUnknown {
    unsafe fn EstablishPtyHandoff(
        &self,
        input: *mut HANDLE,
        output: *mut HANDLE,
        signal: HANDLE,
        reference: HANDLE,
        server: HANDLE,
        client: HANDLE,
        startup_info: *const TerminalStartupInfo,
    ) -> HRESULT;
}

/// one received console session, ready to become a pane
pub struct Handoff {
    /// read side of the VT stream the console host renders into
    pub reader: OwnedHandle,
    /// write side for keystrokes back to the console host
    pub writer: OwnedHandle,
    /// ConPTY signal pipe (resize packets go here)
    pub signal: OwnedHandle,
    /// console driver reference: held open to keep the session alive
    pub reference: OwnedHandle,
    /// the console host (conhost/OpenConsole) process
    pub server: OwnedHandle,
    /// the client command-line application process
    pub client: OwnedHandle,
    /// startup title (usually the client's path or shortcut name)
    pub title: String,
}

// safety: raw HANDLEs moved across threads; all of these are process-local
// kernel handles, safe to use from any thread
unsafe impl Send for Handoff {}

/// where a received session goes: a channel for the `-Embedding` cold path, a
/// proxy hop into the event loop for a running instance
type Deliver = Arc<dyn Fn(Handoff) -> bool + Send + Sync>;

#[implement(ITerminalHandoff3)]
struct TerminalHandoff {
    deliver: Deliver,
}

impl ITerminalHandoff3_Impl for TerminalHandoff_Impl {
    unsafe fn EstablishPtyHandoff(
        &self,
        input: *mut HANDLE,
        output: *mut HANDLE,
        signal: HANDLE,
        reference: HANDLE,
        server: HANDLE,
        client: HANDLE,
        startup_info: *const TerminalStartupInfo,
    ) -> HRESULT {
        // pipe A: console host reads its VT input from a_read, termie writes
        // keystrokes into a_write. pipe B: console host writes rendered VT into
        // b_write, termie reads from b_read
        let (mut a_read, mut a_write) = (HANDLE::default(), HANDLE::default());
        let (mut b_read, mut b_write) = (HANDLE::default(), HANDLE::default());
        unsafe {
            if CreatePipe(&mut a_read, &mut a_write, None, 0).is_err()
                || CreatePipe(&mut b_read, &mut b_write, None, 0).is_err()
                || input.is_null()
                || output.is_null()
            {
                return E_FAIL;
            }
            // hand the console host its pipe ends. these are [out] system_handle
            // params: the RPC stub duplicates them into the console process
            // *after* this method returns, so they must stay open until then —
            // closing them here (before marshaling) hands the console dead
            // handles and it falls back to a legacy window. we keep no copy, so
            // the one open handle each is released when the console closes its
            // duplicate and this side never touches them again
            *input = a_read;
            *output = b_write;
        }
        let title = unsafe {
            startup_info
                .as_ref()
                .map(|si| read_wide(si.pszTitle))
                .unwrap_or_default()
        };
        let h = unsafe {
            Handoff {
                reader: OwnedHandle::from_raw_handle(b_read.0),
                writer: OwnedHandle::from_raw_handle(a_write.0),
                signal: OwnedHandle::from_raw_handle(signal.0),
                reference: OwnedHandle::from_raw_handle(reference.0),
                server: OwnedHandle::from_raw_handle(server.0),
                client: OwnedHandle::from_raw_handle(client.0),
                title,
            }
        };
        log::info!("defterm: received console handoff (title={:?})", h.title);
        // refused deliveries fall back to a plain console window
        if (self.deliver)(h) {
            S_OK
        } else {
            log::warn!("defterm: handoff delivery refused");
            E_FAIL
        }
    }
}

/// set permissive COM security on the main thread before winit initializes OLE
/// drag-drop — otherwise COM locks in authenticated defaults and OpenConsole
/// (MSIX package identity) is denied when it activates our handoff class. call
/// once at process start, only when serving as the default terminal. inits the
/// apartment as STA to match winit, so its later OleInitialize just no-ops
pub fn init_process_security() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
        allow_any_caller();
    }
}

/// disable COM call authentication for this process so a caller in any
/// identity — including OpenConsole running with MSIX package identity — is
/// allowed to activate and call the handoff class object. process-wide and
/// once-only; RPC_E_TOO_LATE (something already set security) is fine to ignore
unsafe fn allow_any_caller() {
    use std::sync::atomic::{AtomicBool, Ordering};
    // CoInitializeSecurity is per-process and once-only: when
    // init_process_security already ran (defterm registered at launch), the
    // serve_running thread's call would just fail RPC_E_TOO_LATE and log a
    // scary-looking line — skip it instead
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let hr = unsafe {
        CoInitializeSecurity(
            None,
            -1,
            None,
            None,
            RPC_C_AUTHN_LEVEL_NONE,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
            None,
        )
    };
    match hr {
        Ok(()) => log::info!("defterm: COM security set (any caller allowed)"),
        Err(e) => log::info!("defterm: CoInitializeSecurity: {e}"),
    }
}

/// NUL-terminated wide string to String; empty for a null pointer
unsafe fn read_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    // titles are short; cap the scan so a torn pointer can't run away
    while len < 4096 && unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

#[implement(IClassFactory)]
struct Factory {
    deliver: Deliver,
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        _outer: Ref<'_, IUnknown>,
        iid: *const GUID,
        object: *mut *mut core::ffi::c_void,
    ) -> windows::core::Result<()> {
        let handoff: ITerminalHandoff3 = TerminalHandoff { deliver: self.deliver.clone() }.into();
        unsafe { handoff.query(iid, object).ok() }
    }

    fn LockServer(&self, _lock: windows::core::BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

/// run the COM local server for a `-Embedding` launch: register the class
/// factory, wait for the console host to call EstablishPtyHandoff, and return
/// the session. None when nothing connects (stale activation) — exit quietly
pub fn serve_embedding() -> Option<Handoff> {
    unsafe {
        // MTA: the handoff call lands on an RPC thread while this one waits
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().ok()?;
        allow_any_caller();
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let deliver: Deliver = Arc::new(move |h| tx.try_send(h).is_ok());
    let factory: IClassFactory = Factory { deliver }.into();
    let cookie = unsafe {
        CoRegisterClassObject(
            &CLSID_TERMIE_HANDOFF,
            &factory,
            CLSCTX_LOCAL_SERVER,
            REGCLS_SINGLEUSE,
        )
        .ok()?
    };
    let got = rx.recv_timeout(Duration::from_secs(30)).ok();
    unsafe {
        let _ = CoRevokeClassObject(cookie);
        // release this MTA so the main thread is clean for winit's STA
        // OleInitialize when we go on to build the window
        CoUninitialize();
    }
    got
}

/// serve handoffs from a running termie for as long as the process lives: new
/// console sessions open as tabs in this window instead of cold-starting
/// another process. also covers systems where COM never manages the
/// `-Embedding` launch (per-user LocalServer32 activation can be hardened
/// off) — a live class object is found either way. call once; safe to call
/// again after registering as default terminal (subsequent calls no-op)
pub fn serve_running(deliver: impl Fn(Handoff) -> bool + Send + Sync + 'static) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let deliver: Deliver = Arc::new(deliver);
    std::thread::spawn(move || {
        unsafe {
            if CoInitializeEx(None, COINIT_MULTITHREADED).ok().is_err() {
                return;
            }
            allow_any_caller();
        }
        let factory: IClassFactory = Factory { deliver }.into();
        let registered = unsafe {
            CoRegisterClassObject(
                &CLSID_TERMIE_HANDOFF,
                &factory,
                CLSCTX_LOCAL_SERVER,
                REGCLS_MULTIPLEUSE,
            )
        };
        match &registered {
            Ok(_) => log::info!("defterm: serving handoffs (class object registered)"),
            Err(e) => log::warn!("defterm: CoRegisterClassObject failed: {e}"),
        }
        if registered.is_ok() {
            // the class object must outlive every future activation; the
            // thread parks for the life of the process
            loop {
                std::thread::park();
            }
        }
    });
}
