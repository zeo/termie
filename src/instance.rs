use std::io::{self, Read, Write};

const MAGIC: &[u8; 4] = b"TRM1";
// a remote-control request; older primaries reject it instead of opening a window for it
const REMOTE_MAGIC: &[u8; 4] = b"TRMR";
const MAX_REQUEST: usize = 1024 * 1024;
const MAX_ARGS: usize = 256;
#[cfg(target_os = "linux")]
const LAUNCH_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(windows)]
const LAUNCH_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(target_os = "linux")]
const REMOTE_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(windows)]
struct WinPipe(std::fs::File);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRequest {
    pub args: Vec<String>,
    pub process_cwd: Option<String>,
    pub launch_cwd: Option<String>,
    /// a `termie @` command: answered with a text reply, never opened as a window
    pub remote: bool,
}

pub enum Claim {
    Primary(Server),
    Forwarded,
    Standalone,
}

pub struct Server {
    #[cfg(target_os = "linux")]
    listener: std::os::unix::net::UnixListener,
    // held for the primary's lifetime so a later launch can tell a stale socket from a live one
    #[cfg(target_os = "linux")]
    _lock: std::fs::File,
    #[cfg(windows)]
    first: WinPipe,
}

impl Server {
    /// serve requests until `send` returns None; a Some reply is the remote-control answer
    pub fn start(self, mut send: impl FnMut(LaunchRequest) -> Option<String> + Send + 'static) {
        std::thread::spawn(move || {
            #[cfg(target_os = "linux")]
            serve_linux(self, &mut send);
            #[cfg(windows)]
            serve_windows(self.first, &mut send);
        });
    }
}

pub fn claim(request: &LaunchRequest) -> Claim {
    #[cfg(target_os = "linux")]
    return claim_linux(request);
    #[cfg(windows)]
    return claim_windows(request);
    #[allow(unreachable_code)]
    Claim::Standalone
}

/// send a `termie @` command to the running primary and return its reply; never becomes primary
pub fn remote(args: Vec<String>, process_cwd: Option<String>) -> io::Result<String> {
    let request = LaunchRequest { args, process_cwd, launch_cwd: None, remote: true };
    #[cfg(target_os = "linux")]
    {
        let stream = std::os::unix::net::UnixStream::connect(linux_socket_path()?)?;
        if !linux_peer_is_user(&stream)? {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "launch owner mismatch"));
        }
        configure_linux_stream_with_timeout(&stream, REMOTE_IO_TIMEOUT)?;
        return ask(stream, &request);
    }
    #[cfg(windows)]
    {
        // the primary recreates its pipe instance after each request, so a
        // back-to-back command can land in that gap
        let mut pipe = open_pipe();
        for _ in 0..5 {
            if pipe.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            pipe = open_pipe();
        }
        return ask(pipe?, &request);
    }
    #[allow(unreachable_code)]
    Err(io::Error::new(io::ErrorKind::Unsupported, "remote control needs windows or linux"))
}

/// the ack byte, then for a remote request the reply as a length-prefixed string
fn answer(remote: bool, reply: Option<&str>) -> Vec<u8> {
    let mut out = vec![reply.is_some() as u8];
    if remote && let Some(text) = reply {
        put_string(&mut out, text);
    }
    out
}

fn put_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn encode(request: &LaunchRequest) -> Option<Vec<u8>> {
    if request.args.len() > MAX_ARGS {
        return None;
    }
    let mut out = Vec::new();
    out.extend_from_slice(if request.remote { REMOTE_MAGIC } else { MAGIC });
    match &request.process_cwd {
        Some(cwd) => put_string(&mut out, cwd),
        None => out.extend_from_slice(&u32::MAX.to_le_bytes()),
    }
    match &request.launch_cwd {
        Some(cwd) => put_string(&mut out, cwd),
        None => out.extend_from_slice(&u32::MAX.to_le_bytes()),
    }
    out.extend_from_slice(&(request.args.len() as u32).to_le_bytes());
    for arg in &request.args {
        put_string(&mut out, arg);
    }
    (out.len() <= MAX_REQUEST).then_some(out)
}

fn take_u32(input: &mut &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = input.get(..4)?.try_into().ok()?;
    *input = &input[4..];
    Some(u32::from_le_bytes(bytes))
}

fn take_string(input: &mut &[u8]) -> Option<String> {
    let len = take_u32(input)? as usize;
    let bytes = input.get(..len)?;
    *input = &input[len..];
    String::from_utf8(bytes.to_vec()).ok()
}

fn decode(bytes: &[u8]) -> Option<LaunchRequest> {
    let (remote, mut input) = match bytes.strip_prefix(MAGIC) {
        Some(rest) => (false, rest),
        None => (true, bytes.strip_prefix(REMOTE_MAGIC)?),
    };
    let process_cwd_len = take_u32(&mut input)?;
    let process_cwd = if process_cwd_len == u32::MAX {
        None
    } else {
        let bytes = input.get(..process_cwd_len as usize)?;
        input = &input[process_cwd_len as usize..];
        Some(String::from_utf8(bytes.to_vec()).ok()?)
    };
    let cwd_len = take_u32(&mut input)?;
    let launch_cwd = if cwd_len == u32::MAX {
        None
    } else {
        let bytes = input.get(..cwd_len as usize)?;
        input = &input[cwd_len as usize..];
        Some(String::from_utf8(bytes.to_vec()).ok()?)
    };
    let count = take_u32(&mut input)? as usize;
    if count > MAX_ARGS {
        return None;
    }
    let mut args = Vec::with_capacity(count);
    for _ in 0..count {
        args.push(take_string(&mut input)?);
    }
    input.is_empty().then_some(LaunchRequest { args, process_cwd, launch_cwd, remote })
}

#[cfg(target_os = "linux")]
fn read_exact_before_deadline(
    stream: &mut std::os::unix::net::UnixStream,
    mut bytes: &mut [u8],
    deadline: std::time::Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "launch request timed out"))?;
        stream.set_read_timeout(Some(remaining))?;
        let read = match stream.read(bytes) {
            Err(error) if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "launch request timed out"));
            }
            result => result?,
        };
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "launch request closed"));
        }
        bytes = &mut bytes[read..];
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_request_with_timeout(
    stream: &mut std::os::unix::net::UnixStream,
    timeout: std::time::Duration,
) -> io::Result<LaunchRequest> {
    let deadline = std::time::Instant::now() + timeout;
    let mut len = [0u8; 4];
    read_exact_before_deadline(stream, &mut len, deadline)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_REQUEST {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "launch request is too large"));
    }
    let mut bytes = vec![0u8; len];
    read_exact_before_deadline(stream, &mut bytes, deadline)?;
    decode(&bytes).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid launch request"))
}

#[cfg(target_os = "linux")]
fn read_request(stream: &mut std::os::unix::net::UnixStream) -> io::Result<LaunchRequest> {
    read_request_with_timeout(stream, LAUNCH_IO_TIMEOUT)
}

fn forward(mut stream: impl Read + Write, request: &LaunchRequest) -> io::Result<()> {
    let bytes = encode(request)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "launch request is too large"))?;
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    let mut ack = [0u8; 1];
    stream.read_exact(&mut ack)?;
    if ack == [1] {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::ConnectionAborted, "launch was rejected"))
    }
}

fn ask(mut stream: impl Read + Write, request: &LaunchRequest) -> io::Result<String> {
    forward(&mut stream, request)?;
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_REQUEST {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "remote reply is too large"));
    }
    let mut text = vec![0u8; len];
    stream.read_exact(&mut text)?;
    String::from_utf8(text).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "remote reply is not utf-8"))
}

// a path socket in the private runtime dir, never the abstract namespace: abstract
// names follow the network namespace, so a plugin jailed with --share-net could reach them
#[cfg(target_os = "linux")]
fn linux_socket_path() -> io::Result<std::path::PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    let dir = runtime.join("termie");
    make_private_dir(&dir)?;
    let session = std::env::var("XDG_SESSION_ID")
        .or_else(|_| std::env::var("WAYLAND_DISPLAY"))
        .or_else(|_| std::env::var("DISPLAY"))
        .unwrap_or_else(|_| "desktop".to_string());
    let session: String = session
        .bytes()
        .map(|byte| if byte.is_ascii_alphanumeric() { byte as char } else { '_' })
        .take(48)
        .collect();
    Ok(dir.join(format!("launch-{session}.sock")))
}

#[cfg(target_os = "linux")]
fn make_private_dir(dir: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "launch dir is not ours"));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_peer_is_user(stream: &std::os::unix::net::UnixStream) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid == unsafe { libc::geteuid() })
}

#[cfg(target_os = "linux")]
fn configure_linux_stream(stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
    configure_linux_stream_with_timeout(stream, LAUNCH_IO_TIMEOUT)
}

#[cfg(target_os = "linux")]
fn configure_linux_stream_with_timeout(
    stream: &std::os::unix::net::UnixStream,
    timeout: std::time::Duration,
) -> io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))
}

#[cfg(target_os = "linux")]
fn try_forward_linux(socket: &std::path::Path, request: &LaunchRequest) -> io::Result<()> {
    let stream = std::os::unix::net::UnixStream::connect(socket)?;
    if !linux_peer_is_user(&stream)? {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "launch owner mismatch"));
    }
    configure_linux_stream(&stream)?;
    forward(stream, request)
}

#[cfg(target_os = "linux")]
fn claim_linux(request: &LaunchRequest) -> Claim {
    match linux_socket_path() {
        Ok(socket) => claim_linux_at(&socket, request),
        Err(_) => Claim::Standalone,
    }
}

#[cfg(target_os = "linux")]
fn claim_linux_at(socket: &std::path::Path, request: &LaunchRequest) -> Claim {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    if try_forward_linux(socket, request).is_ok() {
        return Claim::Forwarded;
    }
    let Ok(lock) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(socket.with_extension("lock"))
    else {
        return Claim::Standalone;
    };
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        // a live primary holds the lock, so its socket is about to accept
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if try_forward_linux(socket, request).is_ok() {
                return Claim::Forwarded;
            }
        }
        return Claim::Standalone;
    }
    let _ = std::fs::remove_file(socket);
    match std::os::unix::net::UnixListener::bind(socket) {
        Ok(listener) => Claim::Primary(Server { listener, _lock: lock }),
        Err(_) => Claim::Standalone,
    }
}

#[cfg(target_os = "linux")]
fn serve_linux(server: Server, send: &mut impl FnMut(LaunchRequest) -> Option<String>) {
    loop {
        let mut stream = match server.listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if !matches!(linux_peer_is_user(&stream), Ok(true)) {
            continue;
        }
        if configure_linux_stream(&stream).is_err() {
            continue;
        }
        let request = match read_request(&mut stream) {
            Ok(request) => request,
            Err(_) => continue,
        };
        let remote = request.remote;
        let reply = send(request);
        let _ = stream.write_all(&answer(remote, reply.as_deref()));
        if reply.is_none() {
            break;
        }
    }
}

#[cfg(windows)]
fn pipe_name() -> String {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;

    let mut session = 0u32;
    let _ = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    format!(r"\\.\pipe\termie-launch-{session}")
}

#[cfg(windows)]
fn open_pipe() -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).write(true).open(pipe_name())
}

#[cfg(windows)]
struct PipeSecurity(windows::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(self.0.0)));
        }
    }
}

#[cfg(windows)]
fn pipe_security() -> io::Result<PipeSecurity> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;

    let sddl: Vec<u16> = "D:P(A;;GRGW;;;OW)".encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
    }
    Ok(PipeSecurity(descriptor))
}

#[cfg(windows)]
fn create_pipe(first: bool) -> io::Result<WinPipe> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::core::PCWSTR;

    let name: Vec<u16> = pipe_name().encode_utf16().chain(std::iter::once(0)).collect();
    let flags = if first {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED
    } else {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED
    };
    let security = pipe_security()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0.0,
        bInheritHandle: false.into(),
    };
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            flags,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            50,
            Some(&attributes),
        )
    };
    if handle.is_invalid() {
        return Err(io::Error::last_os_error());
    }
    Ok(WinPipe(unsafe { std::fs::File::from_raw_handle(handle.0) }))
}

#[cfg(windows)]
fn pipe_handle(pipe: &WinPipe) -> windows::Win32::Foundation::HANDLE {
    use std::os::windows::io::AsRawHandle;

    windows::Win32::Foundation::HANDLE(pipe.0.as_raw_handle())
}

#[cfg(windows)]
fn pipe_io(
    pipe: &WinPipe,
    timeout: std::time::Duration,
    start: impl FnOnce(
        windows::Win32::Foundation::HANDLE,
        &mut windows::Win32::System::IO::OVERLAPPED,
    ) -> windows::core::Result<()>,
) -> io::Result<u32> {
    use windows::Win32::{
        Foundation::{CloseHandle, ERROR_IO_PENDING, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
            Threading::{CreateEventW, WaitForSingleObject},
        },
    };

    let handle = pipe_handle(pipe);
    let event = unsafe { CreateEventW(None, true, false, None) }.map_err(|e| io::Error::other(e.to_string()))?;
    let mut operation = OVERLAPPED { hEvent: event, ..Default::default() };
    let started = start(handle, &mut operation);
    let pending = matches!(&started, Err(error) if error.code() == ERROR_IO_PENDING.to_hresult());
    if let Err(error) = started
        && !pending
    {
        unsafe {
            let _ = CloseHandle(event);
        }
        return Err(io::Error::other(error.to_string()));
    }
    if pending {
        match unsafe { WaitForSingleObject(event, timeout.as_millis().min(u32::MAX as u128) as u32) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => {
                unsafe {
                    let _ = CancelIoEx(handle, Some(&operation));
                    let mut ignored = 0;
                    let _ = GetOverlappedResult(handle, &operation, &mut ignored, true);
                    let _ = CloseHandle(event);
                }
                return Err(io::Error::new(io::ErrorKind::TimedOut, "launch pipe I/O timed out"));
            }
            status => {
                unsafe {
                    let _ = CancelIoEx(handle, Some(&operation));
                    let mut ignored = 0;
                    let _ = GetOverlappedResult(handle, &operation, &mut ignored, true);
                    let _ = CloseHandle(event);
                }
                return Err(io::Error::other(format!("launch pipe wait failed: {status:?}")));
            }
        }
    }
    let mut transferred = 0;
    let result = unsafe { GetOverlappedResult(handle, &operation, &mut transferred, false) }
        .map(|_| transferred)
        .map_err(|e| io::Error::other(e.to_string()));
    unsafe {
        let _ = CloseHandle(event);
    }
    result
}

#[cfg(windows)]
fn read_pipe_exact_before_deadline(
    pipe: &WinPipe,
    mut bytes: &mut [u8],
    deadline: std::time::Instant,
) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::ReadFile;

    while !bytes.is_empty() {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "launch request timed out"))?;
        let read = pipe_io(pipe, remaining, |handle, operation| unsafe {
            ReadFile(handle, Some(bytes), None, Some(operation))
        })? as usize;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "launch pipe closed"));
        }
        bytes = &mut bytes[read..];
    }
    Ok(())
}

#[cfg(windows)]
fn write_pipe_all(pipe: &WinPipe, mut bytes: &[u8]) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::WriteFile;

    while !bytes.is_empty() {
        let written = pipe_io(pipe, LAUNCH_IO_TIMEOUT, |handle, operation| unsafe {
            WriteFile(handle, Some(bytes), None, Some(operation))
        })? as usize;
        if written == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "launch pipe closed"));
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

#[cfg(windows)]
fn read_pipe_request(pipe: &WinPipe) -> io::Result<LaunchRequest> {
    let deadline = std::time::Instant::now() + LAUNCH_IO_TIMEOUT;
    let mut len = [0u8; 4];
    read_pipe_exact_before_deadline(pipe, &mut len, deadline)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_REQUEST {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "launch request is too large"));
    }
    let mut bytes = vec![0u8; len];
    read_pipe_exact_before_deadline(pipe, &mut bytes, deadline)?;
    decode(&bytes).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid launch request"))
}

#[cfg(windows)]
fn connect_pipe(pipe: &WinPipe) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::{CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE},
        System::{
            IO::{GetOverlappedResult, OVERLAPPED},
            Pipes::ConnectNamedPipe,
            Threading::{CreateEventW, WaitForSingleObject},
        },
    };

    let handle = HANDLE(pipe.0.as_raw_handle());
    let event = unsafe { CreateEventW(None, true, false, None) }.map_err(|e| io::Error::other(e.to_string()))?;
    let mut operation = OVERLAPPED { hEvent: event, ..Default::default() };
    let result = match unsafe { ConnectNamedPipe(handle, Some(&mut operation)) } {
        Ok(()) => Ok(()),
        Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => Ok(()),
        Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
            let _ = unsafe { WaitForSingleObject(event, u32::MAX) };
            let mut ignored = 0;
            unsafe { GetOverlappedResult(handle, &operation, &mut ignored, false) }
                .map_err(|e| io::Error::other(e.to_string()))
        }
        Err(error) => Err(io::Error::other(error.to_string())),
    };
    unsafe {
        let _ = CloseHandle(event);
    }
    result
}

#[cfg(windows)]
fn claim_windows(request: &LaunchRequest) -> Claim {
    if let Ok(stream) = open_pipe()
        && forward(stream, request).is_ok()
    {
        return Claim::Forwarded;
    }
    match create_pipe(true) {
        Ok(first) => Claim::Primary(Server { first }),
        Err(_) => {
            for _ in 0..5 {
                std::thread::sleep(std::time::Duration::from_millis(10));
                if let Ok(stream) = open_pipe()
                    && forward(stream, request).is_ok()
                {
                    return Claim::Forwarded;
                }
            }
            Claim::Standalone
        }
    }
}

#[cfg(windows)]
fn serve_windows(mut stream: WinPipe, send: &mut impl FnMut(LaunchRequest) -> Option<String>) {
    loop {
        if connect_pipe(&stream).is_err() {
            return;
        }
        let accepted = match read_pipe_request(&stream) {
            Ok(request) => {
                let remote = request.remote;
                let reply = send(request);
                let _ = write_pipe_all(&stream, &answer(remote, reply.as_deref()));
                reply.is_some()
            }
            Err(_) => true,
        };
        if !accepted {
            return;
        }
        drop(stream);
        match create_pipe(false) {
            Ok(next) => stream = next,
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_requests_round_trip_empty_and_complex_arguments() {
        for request in [
            LaunchRequest { args: Vec::new(), process_cwd: None, launch_cwd: None, remote: false },
            LaunchRequest {
                args: vec!["--cwd".into(), "D:\\work tree".into(), "--".into(), "pwsh".into()],
                process_cwd: Some("D:\\repo".into()),
                launch_cwd: Some("/home/me/project".into()),
                remote: false,
            },
            LaunchRequest { args: vec!["list".into()], process_cwd: Some("/tmp".into()), launch_cwd: None, remote: true },
        ] {
            assert_eq!(decode(&encode(&request).unwrap()), Some(request));
        }
    }

    #[test]
    fn malformed_and_oversized_launch_requests_are_rejected() {
        assert!(decode(b"TRM1").is_none());
        let request = LaunchRequest {
            args: vec!["x".repeat(MAX_REQUEST)],
            process_cwd: None,
            launch_cwd: None,
            remote: false,
        };
        assert!(encode(&request).is_none());
    }

    struct Duplex {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn remote_requests_get_a_text_reply_and_launches_only_an_ack() {
        assert_eq!(answer(false, Some("ignored")), [1]);
        assert_eq!(answer(true, None), [0]);
        let request = LaunchRequest { args: vec!["list".into()], process_cwd: None, launch_cwd: None, remote: true };
        let mut stream = Duplex { input: io::Cursor::new(answer(true, Some("pane 1 focused"))), output: Vec::new() };
        assert_eq!(ask(&mut stream, &request).unwrap(), "pane 1 focused");
        let sent = encode(&request).unwrap();
        assert_eq!(stream.output[4..], sent[..]);
        assert!(sent.starts_with(REMOTE_MAGIC));
        let mut refused = Duplex { input: io::Cursor::new(answer(true, None)), output: Vec::new() };
        assert!(ask(&mut refused, &request).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_launch_streams_have_a_finite_io_budget() {
        let (stream, _) = std::os::unix::net::UnixStream::pair().unwrap();
        configure_linux_stream(&stream).unwrap();
        assert_eq!(stream.read_timeout().unwrap(), Some(LAUNCH_IO_TIMEOUT));
        assert_eq!(stream.write_timeout().unwrap(), Some(LAUNCH_IO_TIMEOUT));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn incomplete_linux_launch_request_times_out() {
        use std::time::{Duration, Instant};

        let (mut reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        writer.write_all(&8u32.to_le_bytes()).unwrap();
        let started = Instant::now();
        assert!(read_request_with_timeout(&mut reader, Duration::from_millis(25)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn slow_linux_launch_request_uses_one_deadline() {
        use std::time::{Duration, Instant};

        let (mut reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        writer.write_all(&8u32.to_le_bytes()).unwrap();
        writer.write_all(b"x").unwrap();
        let started = Instant::now();
        let error = read_request_with_timeout(&mut reader, Duration::from_millis(25)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(target_os = "linux")]
    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("termie-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        make_private_dir(&dir).unwrap();
        dir
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_second_launch_reclaims_a_stale_socket_then_forwards_to_the_primary() {
        let dir = scratch_dir("launch-claim");
        let socket = dir.join("launch-test.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let request =
            LaunchRequest { args: vec!["--title".into(), "x".into()], process_cwd: None, launch_cwd: None, remote: false };
        let Claim::Primary(server) = claim_linux_at(&socket, &request) else { panic!("stale socket was not reclaimed") };
        let (sent, received) = std::sync::mpsc::channel();
        server.start(move |request| sent.send(request).is_ok().then(String::new));
        assert!(matches!(claim_linux_at(&socket, &request), Claim::Forwarded));
        assert_eq!(received.recv_timeout(std::time::Duration::from_secs(2)).unwrap(), request);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_remote_command_gets_the_primarys_reply_over_the_socket() {
        let dir = scratch_dir("launch-remote");
        let socket = dir.join("launch-test.sock");
        let launch = LaunchRequest { args: Vec::new(), process_cwd: None, launch_cwd: None, remote: false };
        let Claim::Primary(server) = claim_linux_at(&socket, &launch) else { panic!("no primary") };
        server.start(|request| Some(if request.remote { format!("got {}", request.args.join(" ")) } else { String::new() }));
        let request = LaunchRequest { args: vec!["list".into()], process_cwd: None, launch_cwd: None, remote: true };
        let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        configure_linux_stream_with_timeout(&stream, REMOTE_IO_TIMEOUT).unwrap();
        assert_eq!(ask(stream, &request).unwrap(), "got list");
        assert!(matches!(claim_linux_at(&socket, &launch), Claim::Forwarded));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_loose_launch_dir_is_made_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("launch-dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        make_private_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::remove_dir_all(dir).unwrap();
    }

    // runs inside the jail spawned by the test below; a no-op anywhere else
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "helper for plugin_jail_with_network_cannot_reach_the_launch_socket"]
    fn jailed_launch_probe() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::{SocketAddr, UnixStream};

        for arg in std::env::args() {
            let reached = if let Some(path) = arg.strip_prefix("probe-path=") {
                UnixStream::connect(path).is_ok()
            } else if let Some(name) = arg.strip_prefix("probe-abstract=") {
                SocketAddr::from_abstract_name(name).and_then(|addr| UnixStream::connect_addr(&addr)).is_ok()
            } else {
                continue;
            };
            println!("{arg} reached={reached}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs bubblewrap installed"]
    fn plugin_jail_with_network_cannot_reach_the_launch_socket() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::{SocketAddr, UnixListener};

        let dir = scratch_dir("launch-jail");
        let socket = dir.join("launch-test.sock");
        let _path = UnixListener::bind(&socket).unwrap();
        let name = format!("termie-jail-probe-{}", std::process::id());
        let _abstract = UnixListener::bind_addr(&SocketAddr::from_abstract_name(&name).unwrap()).unwrap();
        let exe = std::env::current_exe().unwrap();
        let args = [
            "tests::jailed_launch_probe".to_string(),
            "--ignored".into(),
            "--nocapture".into(),
            "--test-threads=1".into(),
            format!("probe-path={}", socket.display()),
            format!("probe-abstract={name}"),
        ];
        let mut jail = crate::plugin::sandbox::spawn("termie.plugin.selftest", &exe, &args, exe.parent().unwrap(), true)
            .expect("bwrap spawn");
        let mut out = String::new();
        jail.take_stdout().unwrap().read_to_string(&mut out).unwrap();
        jail.kill();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(out.contains(&format!("probe-abstract={name} reached=true")), "control unreachable, probe proves nothing: {out}");
        assert!(out.contains(&format!("probe-path={} reached=false", socket.display())), "jail reached the launch socket: {out}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_launch_streams_have_a_finite_io_budget() {
        assert_eq!(LAUNCH_IO_TIMEOUT, std::time::Duration::from_secs(2));
    }
}
