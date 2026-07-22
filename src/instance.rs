use std::io::{self, Read, Write};

const MAGIC: &[u8; 4] = b"TRM1";
const MAX_REQUEST: usize = 1024 * 1024;
const MAX_ARGS: usize = 256;
#[cfg(target_os = "linux")]
const LAUNCH_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRequest {
    pub args: Vec<String>,
    pub process_cwd: Option<String>,
    pub launch_cwd: Option<String>,
}

pub enum Claim {
    Primary(Server),
    Forwarded,
    Standalone,
}

pub struct Server {
    #[cfg(target_os = "linux")]
    listener: std::os::unix::net::UnixListener,
    #[cfg(windows)]
    first: std::fs::File,
}

impl Server {
    pub fn start(self, mut send: impl FnMut(LaunchRequest) -> bool + Send + 'static) {
        std::thread::spawn(move || {
            #[cfg(target_os = "linux")]
            serve_linux(self.listener, &mut send);
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

fn put_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn encode(request: &LaunchRequest) -> Option<Vec<u8>> {
    if request.args.len() > MAX_ARGS {
        return None;
    }
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
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
    let mut input = bytes.strip_prefix(MAGIC)?;
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
    input.is_empty().then_some(LaunchRequest { args, process_cwd, launch_cwd })
}

fn read_request(stream: &mut impl Read) -> io::Result<LaunchRequest> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_REQUEST {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "launch request is too large"));
    }
    let mut bytes = vec![0u8; len];
    stream.read_exact(&mut bytes)?;
    decode(&bytes).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid launch request"))
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

#[cfg(target_os = "linux")]
fn linux_addr() -> io::Result<std::os::unix::net::SocketAddr> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::SocketAddr;

    let session = std::env::var("XDG_SESSION_ID")
        .or_else(|_| std::env::var("WAYLAND_DISPLAY"))
        .or_else(|_| std::env::var("DISPLAY"))
        .unwrap_or_else(|_| "desktop".to_string());
    let session: String = session
        .bytes()
        .map(|byte| if byte.is_ascii_alphanumeric() { byte as char } else { '_' })
        .take(48)
        .collect();
    let name = format!("termie-launch-{}-{session}", unsafe { libc::geteuid() });
    SocketAddr::from_abstract_name(name)
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
fn try_forward_linux(request: &LaunchRequest) -> io::Result<()> {
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect_addr(&linux_addr()?)?;
    if !linux_peer_is_user(&stream)? {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "launch owner mismatch"));
    }
    configure_linux_stream(&stream)?;
    forward(stream, request)
}

#[cfg(target_os = "linux")]
fn claim_linux(request: &LaunchRequest) -> Claim {
    use std::os::unix::net::UnixListener;

    if try_forward_linux(request).is_ok() {
        return Claim::Forwarded;
    }
    let Ok(addr) = linux_addr() else {
        return Claim::Standalone;
    };
    match UnixListener::bind_addr(&addr) {
        Ok(listener) => Claim::Primary(Server { listener }),
        Err(_) => {
            for _ in 0..5 {
                std::thread::sleep(std::time::Duration::from_millis(10));
                if try_forward_linux(request).is_ok() {
                    return Claim::Forwarded;
                }
            }
            Claim::Standalone
        }
    }
}

#[cfg(target_os = "linux")]
fn serve_linux(listener: std::os::unix::net::UnixListener, send: &mut impl FnMut(LaunchRequest) -> bool) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            break;
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
        let accepted = send(request);
        let _ = stream.write_all(&[accepted as u8]);
        if !accepted {
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
fn create_pipe(first: bool) -> io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows::core::PCWSTR;

    let name: Vec<u16> = pipe_name().encode_utf16().chain(std::iter::once(0)).collect();
    let flags = if first {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        PIPE_ACCESS_DUPLEX
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
            None,
        )
    };
    if handle.is_invalid() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_handle(handle.0) })
}

#[cfg(windows)]
fn connect_pipe(pipe: &std::fs::File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE};
    use windows::Win32::System::Pipes::ConnectNamedPipe;

    let handle = HANDLE(pipe.as_raw_handle());
    match unsafe { ConnectNamedPipe(handle, None) } {
        Ok(()) => Ok(()),
        Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => Ok(()),
        Err(error) => Err(io::Error::other(error.to_string())),
    }
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
fn serve_windows(mut stream: std::fs::File, send: &mut impl FnMut(LaunchRequest) -> bool) {
    loop {
        if connect_pipe(&stream).is_err() {
            return;
        }
        let accepted = match read_request(&mut stream) {
            Ok(request) => {
                let accepted = send(request);
                let _ = stream.write_all(&[accepted as u8]);
                accepted
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
            LaunchRequest { args: Vec::new(), process_cwd: None, launch_cwd: None },
            LaunchRequest {
                args: vec!["--cwd".into(), "D:\\work tree".into(), "--".into(), "pwsh".into()],
                process_cwd: Some("D:\\repo".into()),
                launch_cwd: Some("/home/me/project".into()),
            },
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
        };
        assert!(encode(&request).is_none());
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
        configure_linux_stream_with_timeout(&reader, Duration::from_millis(25)).unwrap();
        writer.write_all(&8u32.to_le_bytes()).unwrap();
        let started = Instant::now();
        assert!(read_request(&mut reader).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
