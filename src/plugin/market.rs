//! plugin marketplace: a curated remote index of installable plugins plus the
//! local install/remove plumbing. archive work uses the OS ZIP extractor.
//! catalog and plugin downloads are pinned to one catalog commit. they use the
//! GitHub CLI when available and fall back to anonymous `curl`.
//!
//! security: downloaded archives are unpacked into a fresh temp dir and the
//! resulting `plugin.json` is validated (id safe + matches the catalog id)
//! BEFORE anything is moved into the plugins directory, so a malicious archive
//! can't traverse out or shadow another plugin.

use std::path::{Component, Path, PathBuf};
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use super::json::Json;
use super::manifest::{id_is_safe, Manifest};

const MAX_INDEX_BYTES: usize = 1024 * 1024;
const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ARCHIVE_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4096;
const MAX_ARCHIVE_LISTING_BYTES: usize = 16 * 1024 * 1024;
const MAX_ARCHIVE_EXTRACT_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);
/// the catalog repo and immutable commit behind every marketplace fetch
const CATALOG_REPO: &str = "zeo/termie-plugins";
const CATALOG_REF: &str = "6b21244e584bc344d21fdafcc3300264dea58364";

fn catalog_raw_url(path: &str) -> String {
    format!("https://raw.githubusercontent.com/{CATALOG_REPO}/{CATALOG_REF}/{path}")
}

/// one catalog entry from the remote index
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// url of the plugin archive (zip) to download
    pub url: String,
    /// permissions the plugin declares it wants (shown before install)
    pub permissions: Vec<String>,
}

/// parse the index JSON: `{ "plugins": [ {id,name,version,description,url,permissions[]}, ... ] }`.
/// entries with an unsafe id are dropped so the catalog can't smuggle a bad id
pub fn parse_index(text: &str) -> Vec<Entry> {
    let Some(json) = Json::parse(text) else {
        return Vec::new();
    };
    let Some(arr) = json.get("plugins").and_then(Json::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let id = e.get_str("id")?.to_string();
            let url = e.get_str("url").unwrap_or("");
            if !marketplace_url_is_safe(url) {
                return None;
            }
            let path = catalog_repo_path(url)?;
            if !id_is_safe(&id) {
                return None;
            }
            Some(Entry {
                id,
                name: e.get_str("name").unwrap_or("").to_string(),
                version: e.get_str("version").unwrap_or("0.0.0").to_string(),
                description: e.get_str("description").unwrap_or("").to_string(),
                url: catalog_raw_url(path),
                permissions: e
                    .get("permissions")
                    .and_then(Json::as_array)
                    .map(|a| a.iter().filter_map(|p| p.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
            })
        })
        .filter(|e| !e.url.is_empty())
        .collect()
}

/// resolve a helper to an absolute path before spawning: CreateProcess searches
/// the exe dir and the process cwd before PATH, and termie's cwd can be an
/// arbitrary repo (an "open in termie" launch) where a planted curl.exe or
/// gh.exe would win. system32 first (curl/tar ship there), then absolute PATH
/// entries only; unresolved names become a clean spawn failure, never a hunt
#[cfg(windows)]
pub(crate) fn system_executable(name: &str) -> Option<std::path::PathBuf> {
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buf = vec![0; 32_768];
    let len = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    (len > 0 && len < buf.len()).then(|| {
        std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len])).join(format!("{name}.exe"))
    })
}

#[cfg(windows)]
fn resolve_helper(name: &str) -> std::path::PathBuf {
    let exe = format!("{name}.exe");
    let sys32 = system_executable(name);
    if let Some(p) = &sys32
        && p.is_file()
    {
        return p.clone();
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if !dir.is_absolute() {
                continue;
            }
            let cand = dir.join(&exe);
            if cand.is_file() {
                return cand;
            }
        }
    }
    sys32.unwrap_or_default()
}

/// build a console command that won't flash a window: termie is a gui app, so a
/// bare gh/curl/tar spawn pops a console window. CREATE_NO_WINDOW suppresses it
#[cfg(windows)]
pub(crate) fn quiet_command(program: &str) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new(resolve_helper(program));
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}
#[cfg(not(windows))]
fn unix_helper_path(program: &str, path: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    let program_path = Path::new(program);
    if program_path.is_absolute() {
        return Some(program_path.to_path_buf());
    }
    if let Some(path) = path {
        for dir in std::env::split_paths(&path) {
            if !dir.is_absolute() {
                continue;
            }
            let candidate = dir.join(program);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for dir in ["/usr/bin", "/bin"] {
        let candidate = Path::new(dir).join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(not(windows))]
pub(crate) fn quiet_command(program: &str) -> Command {
    Command::new(unix_helper_path(program, std::env::var_os("PATH")).unwrap_or_default())
}

fn archive_path_is_safe(path: &str) -> bool {
    let archive_path = Path::new(path);
    !path.contains(['\\', ':'])
        && !archive_path.as_os_str().is_empty()
        && !archive_path.is_absolute()
        && archive_path.components().all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
}

fn catalog_repo_path(url: &str) -> Option<&str> {
    let prefix = format!("https://raw.githubusercontent.com/{CATALOG_REPO}/");
    let rest = url.strip_prefix(&prefix)?;
    if rest.bytes().any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        || rest.contains(['\\', '#', '?', '@'])
    {
        return None;
    }
    let (reference, path) = rest.split_once('/')?;
    if reference != "main" && reference != CATALOG_REF {
        return None;
    }
    let repo_path = Path::new(path);
    if repo_path.as_os_str().is_empty()
        || repo_path.is_absolute()
        || !repo_path.components().all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    Some(path)
}

fn marketplace_url_is_safe(url: &str) -> bool {
    let Some(path) = catalog_repo_path(url) else {
        return false;
    };
    path.ends_with(".zip")
}

fn marketplace_curl() -> Command {
    let mut command = quiet_command("curl");
    command.args(["-q", "--globoff", "--proto", "=https", "--proto-redir", "=https"]);
    command
}

fn validate_archive_listing(bytes: Vec<u8>) -> Result<usize, String> {
    let listing = String::from_utf8(bytes).map_err(|_| "archive contains non-UTF-8 paths")?;
    let mut entries = 0;
    for path in listing.lines() {
        if !archive_path_is_safe(path) {
            return Err("archive contains an unsafe path".to_string());
        }
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            return Err("archive contains too many entries".to_string());
        }
    }
    (entries > 0).then_some(entries).ok_or_else(|| "archive is empty".to_string())
}

#[cfg(any(windows, test))]
fn validate_tar_types(bytes: Vec<u8>, expected: usize) -> Result<(), String> {
    let listing = String::from_utf8(bytes).map_err(|_| "archive contains non-UTF-8 metadata")?;
    let mut entries = 0;
    let mut extracted_bytes = 0u64;
    for line in listing.lines() {
        if line.is_empty() {
            continue;
        }
        entries += 1;
        if !matches!(line.as_bytes().first(), Some(b'-' | b'd')) {
            return Err("archive contains a link or special file".to_string());
        }
        let size = line
            .split_ascii_whitespace()
            .nth(4)
            .and_then(|field| field.parse::<u64>().ok())
            .ok_or_else(|| "archive contains unreadable size metadata".to_string())?;
        extracted_bytes = extracted_bytes
            .checked_add(size)
            .filter(|total| *total <= MAX_ARCHIVE_EXTRACTED_BYTES)
            .ok_or_else(|| "archive expands past the 256 MiB limit".to_string())?;
    }
    (entries == expected)
        .then_some(())
        .ok_or_else(|| "archive metadata does not match its file list".to_string())
}

#[cfg(any(not(windows), test))]
fn zipinfo_entry(line: &str) -> Option<(u8, u64)> {
    let mut fields = line.split_ascii_whitespace();
    let attributes = fields.next()?.as_bytes();
    let version = fields.next()?;
    let (major, minor) = version.split_once('.')?;
    if major.is_empty()
        || minor.is_empty()
        || !major.bytes().all(|byte| byte.is_ascii_digit())
        || !minor.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    fields.next()?;
    let size = fields.next()?.parse().ok()?;
    Some((attributes.first().copied()?, size))
}

#[cfg(any(not(windows), test))]
fn validate_zipinfo_types(bytes: Vec<u8>, expected: usize) -> Result<(), String> {
    let listing = String::from_utf8(bytes).map_err(|_| "archive contains non-UTF-8 metadata")?;
    let mut entries = 0;
    let mut extracted_bytes = 0u64;
    for (kind, size) in listing.lines().filter_map(zipinfo_entry) {
        entries += 1;
        if !matches!(kind, b'-' | b'd') {
            return Err("archive contains a link or special file".to_string());
        }
        extracted_bytes = extracted_bytes
            .checked_add(size)
            .filter(|total| *total <= MAX_ARCHIVE_EXTRACTED_BYTES)
            .ok_or_else(|| "archive expands past the 256 MiB limit".to_string())?;
    }
    (entries == expected)
        .then_some(())
        .ok_or_else(|| "archive metadata does not match its file list".to_string())
}

#[cfg(windows)]
fn unpack_archive(archive: &Path, unpack: &Path) -> Result<(), String> {
    let mut listing = quiet_command("tar");
    listing
        .env_remove("TAR_OPTIONS")
        .env_remove("TAR_READER_OPTIONS")
        .arg("-tf")
        .arg(archive);
    let listing = bounded_archive_metadata(&mut listing)?;
    if !listing.status.success() {
        return Err(format!("couldn't inspect archive: status {:?}", listing.status));
    }
    let entries = validate_archive_listing(listing.stdout)?;
    let mut kinds = quiet_command("tar");
    kinds
        .env_remove("TAR_OPTIONS")
        .env_remove("TAR_READER_OPTIONS")
        .arg("-tvf")
        .arg(archive);
    let kinds = bounded_archive_metadata(&mut kinds)?;
    if !kinds.status.success() {
        return Err(format!("couldn't inspect archive types: status {:?}", kinds.status));
    }
    validate_tar_types(kinds.stdout, entries)?;
    let mut extract = quiet_command("tar");
    extract
        .env_remove("TAR_OPTIONS")
        .env_remove("TAR_READER_OPTIONS")
        .args(["--no-same-owner", "--no-same-permissions"])
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(unpack);
    let output = bounded_output(&mut extract, MAX_ARCHIVE_EXTRACT_OUTPUT_BYTES).map_err(|error| match error {
        BoundedOutputError::Io(error) => format!("couldn't run tar: {error}"),
        BoundedOutputError::Limit => "archive extraction output exceeds the 64 KiB limit".to_string(),
    })?;
    output
        .status
        .success()
        .then_some(())
        .ok_or_else(|| format!("unpack failed: status {:?}", output.status))
}

#[cfg(not(windows))]
fn unpack_archive(archive: &Path, unpack: &Path) -> Result<(), String> {
    let mut listing = quiet_command("unzip");
    listing
        .env_remove("ZIPINFO")
        .env_remove("ZIPINFOOPT")
        .arg("-Z1")
        .arg(archive);
    let listing = bounded_archive_metadata(&mut listing)?;
    if !listing.status.success() {
        return Err(format!("couldn't inspect archive: status {:?}", listing.status));
    }
    let entries = validate_archive_listing(listing.stdout)?;
    let mut kinds = quiet_command("unzip");
    kinds
        .env_remove("ZIPINFO")
        .env_remove("ZIPINFOOPT")
        .args(["-Z", "-s"])
        .arg(archive);
    let kinds = bounded_archive_metadata(&mut kinds)?;
    if !kinds.status.success() {
        return Err(format!("couldn't inspect archive types: status {:?}", kinds.status));
    }
    validate_zipinfo_types(kinds.stdout, entries)?;
    let mut extract = quiet_command("unzip");
    extract
        .env_remove("UNZIP")
        .env_remove("UNZIPOPT")
        .arg("-qq")
        .arg(archive)
        .arg("-d")
        .arg(unpack);
    let output = bounded_output(&mut extract, MAX_ARCHIVE_EXTRACT_OUTPUT_BYTES).map_err(|error| match error {
        BoundedOutputError::Io(error) => format!("couldn't run unzip: {error}"),
        BoundedOutputError::Limit => "archive extraction output exceeds the 64 KiB limit".to_string(),
    })?;
    output
        .status
        .success()
        .then_some(())
        .ok_or_else(|| format!("unpack failed: status {:?}", output.status))
}

fn reject_symlinks(dir: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("inspect archive: {e}"))? {
        let entry = entry.map_err(|e| format!("inspect archive: {e}"))?;
        let kind = entry.file_type().map_err(|e| format!("inspect archive: {e}"))?;
        if kind.is_symlink() {
            return Err(format!("archive contains symlink {:?}", entry.file_name()));
        }
        if kind.is_dir() {
            reject_symlinks(&entry.path())?;
        }
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum BoundedOutputError {
    Io(std::io::Error),
    Limit,
}

#[cfg(windows)]
pub(crate) struct ProcessTree(Option<windows::Win32::Foundation::HANDLE>);

// job handles have no thread affinity and are closed by their owner
#[cfg(windows)]
unsafe impl Send for ProcessTree {}

#[cfg(windows)]
impl ProcessTree {
    pub(crate) fn attach(child: &std::process::Child) -> Self {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;

        Self::attach_handle(HANDLE(child.as_raw_handle() as *mut _))
    }

    pub(crate) fn attach_handle(process: windows::Win32::Foundation::HANDLE) -> Self {
        use windows::Win32::{
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };

        let mut job = Self(unsafe { CreateJobObjectW(None, None).ok() });
        let Some(handle) = job.0 else {
            return job;
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                std::mem::size_of_val(&limits).try_into().unwrap(),
            )
            .and_then(|_| AssignProcessToJobObject(handle, process))
            .is_ok()
        };
        if !configured {
            job.close();
        }
        job
    }

    pub(crate) fn terminate(&mut self) {
        use windows::Win32::System::JobObjects::TerminateJobObject;

        if let Some(handle) = self.0 {
            let _ = unsafe { TerminateJobObject(handle, 1) };
        }
    }

    fn close(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(not(windows))]
pub(crate) struct ProcessTree;

#[cfg(not(windows))]
impl ProcessTree {
    pub(crate) fn attach(_child: &std::process::Child) -> Self {
        Self
    }

    pub(crate) fn terminate(&mut self) {}
}

fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>, BoundedOutputError> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(BoundedOutputError::Io)?;
    if bytes.len() > limit {
        return Err(BoundedOutputError::Limit);
    }
    Ok(bytes)
}

pub(crate) fn bounded_output(command: &mut Command, limit: usize) -> Result<Output, BoundedOutputError> {
    bounded_output_with_deadline(command, limit, HELPER_TIMEOUT)
}

pub(crate) fn bounded_output_with_deadline(
    command: &mut Command,
    limit: usize,
    timeout: Duration,
) -> Result<Output, BoundedOutputError> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(BoundedOutputError::Io)?;
    let mut job = ProcessTree::attach(&child);
    let stdout = child.stdout.take().expect("piped child stdout");
    let mut stderr = child.stderr.take().expect("piped child stderr");
    let (stdout_tx, stdout_rx) = mpsc::sync_channel(1);
    let stdout_task = std::thread::spawn(move || {
        let _ = stdout_tx.send(read_bounded(stdout, limit));
    });
    let stderr_task = std::thread::spawn(move || {
        let mut kept = Vec::new();
        {
            let mut prefix = stderr.by_ref().take(64 * 1024);
            prefix.read_to_end(&mut kept)?;
        }
        std::io::copy(&mut stderr, &mut std::io::sink())?;
        Ok::<_, std::io::Error>(kept)
    });

    let deadline = Instant::now() + timeout;
    let mut stdout = None;
    let status = loop {
        if stdout.is_none() {
            match stdout_rx.try_recv() {
                Ok(Ok(bytes)) => stdout = Some(bytes),
                Ok(Err(error)) => {
                    kill_helper(&mut child, &mut job);
                    let _ = child.wait();
                    let _ = stdout_task.join();
                    let _ = stderr_task.join();
                    return Err(error);
                }
                Err(TryRecvError::Disconnected) => {
                    kill_helper(&mut child, &mut job);
                    let _ = child.wait();
                    let _ = stdout_task.join();
                    let _ = stderr_task.join();
                    return Err(BoundedOutputError::Io(std::io::Error::other("stdout reader panicked")));
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                kill_helper(&mut child, &mut job);
                let _ = child.wait();
                let _ = stdout_task.join();
                let _ = stderr_task.join();
                return Err(BoundedOutputError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "helper exceeded its deadline",
                )));
            }
            Err(error) => {
                kill_helper(&mut child, &mut job);
                let _ = child.wait();
                let _ = stdout_task.join();
                let _ = stderr_task.join();
                return Err(BoundedOutputError::Io(error));
            }
        }
    };
    let stdout = match stdout {
        Some(stdout) => stdout,
        None => stdout_rx
            .recv()
            .map_err(|_| BoundedOutputError::Io(std::io::Error::other("stdout reader panicked")))??,
    };
    let _ = stdout_task.join();
    let stderr = stderr_task
        .join()
        .map_err(|_| BoundedOutputError::Io(std::io::Error::other("stderr reader panicked")))?
        .map_err(BoundedOutputError::Io)?;
    Ok(Output { status, stdout, stderr })
}

fn kill_helper(child: &mut std::process::Child, job: &mut ProcessTree) {
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    job.terminate();
    let _ = child.kill();
}

fn bounded_archive_metadata(command: &mut Command) -> Result<Output, String> {
    bounded_output(command, MAX_ARCHIVE_LISTING_BYTES).map_err(|error| match error {
        BoundedOutputError::Io(error) => format!("couldn't inspect archive: {error}"),
        BoundedOutputError::Limit => "archive metadata exceeds the 16 MiB limit".to_string(),
    })
}

/// fetch raw bytes from the pinned catalog commit, preferring an authenticated
/// GitHub CLI request and falling back to the matching immutable raw URL
fn fetch_bytes(url: &str, limit: usize) -> Result<Vec<u8>, String> {
    let path = catalog_repo_path(url).ok_or_else(|| "marketplace URL is outside the pinned catalog".to_string())?;
    let api = format!("repos/{CATALOG_REPO}/contents/{path}?ref={CATALOG_REF}");
    let mut command = quiet_command("gh");
    command.args(["api", &api, "-H", "Accept: application/vnd.github.raw"]);
    match bounded_output(&mut command, limit) {
        Ok(o) if o.status.success() => return Ok(o.stdout),
        Ok(o) => log::warn!("gh fetch of {path} failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
        Err(BoundedOutputError::Limit) => return Err(format!("response exceeds {limit} byte limit")),
        Err(BoundedOutputError::Io(e)) => log::warn!("gh unavailable ({e}); trying curl"),
    }
    let pinned_url = catalog_raw_url(path);
    let mut command = marketplace_curl();
    command.args(["-fsSL", "--max-time", "60", "--", &pinned_url]);
    match bounded_output(&mut command, limit) {
        Ok(o) if o.status.success() => Ok(o.stdout),
        Ok(_) => Err("couldn't reach the pinned plugin catalog".to_string()),
        Err(BoundedOutputError::Limit) => Err(format!("response exceeds {limit} byte limit")),
        Err(BoundedOutputError::Io(e)) => Err(format!("couldn't run curl: {e}")),
    }
}

fn read_manifest(path: &Path) -> Result<String, String> {
    let manifest_bytes = read_bounded(
        std::fs::File::open(path).map_err(|e| format!("manifest: {e}"))?,
        MAX_MANIFEST_BYTES,
    )
    .map_err(|error| match error {
        BoundedOutputError::Io(error) => format!("manifest: {error}"),
        BoundedOutputError::Limit => format!("manifest exceeds the {} KiB limit", MAX_MANIFEST_BYTES / 1024),
    })?;
    String::from_utf8(manifest_bytes).map_err(|_| "manifest is not UTF-8".to_string())
}

/// fetch + parse the catalog index. Ok(entries) on a successful fetch (possibly
/// empty if the catalog is), Err(reason) if the request itself failed — so the
/// store can tell "empty" from "unreachable"
pub fn fetch_index() -> Result<Vec<Entry>, String> {
    let bytes = fetch_bytes(&catalog_raw_url("index.json"), MAX_INDEX_BYTES)?;
    Ok(parse_index(&String::from_utf8_lossy(&bytes)))
}

/// download + install `entry` into `plugins_dir`. downloads the archive to a
/// temp file, unpacks into a temp dir, validates the manifest (id safe + equals
/// the catalog id), then atomically swaps it into `plugins_dir/<id>`. returns
/// the installed manifest on success
pub fn install(entry: &Entry, plugins_dir: &Path, temp_dir: &Path) -> Result<Manifest, String> {
    if !id_is_safe(&entry.id) {
        return Err(format!("unsafe plugin id {:?}", entry.id));
    }
    let work = fresh_work_dir(temp_dir, &entry.id)?;
    let result = (|| {
        let archive = work.join("plugin.zip");
        let unpack = work.join("unpack");
        std::fs::create_dir(&unpack).map_err(|e| format!("unpack dir: {e}"))?;

        // download (authenticated via gh for the private catalog repo, else curl)
        let bytes = fetch_bytes(&entry.url, MAX_ARCHIVE_BYTES)
            .map_err(|e| format!("download failed: {e}"))?;
        std::fs::write(&archive, &bytes).map_err(|e| format!("write archive: {e}"))?;

        unpack_archive(&archive, &unpack)?;
        reject_symlinks(&unpack)?;

        // the archive may wrap its files in a top dir; find the dir containing
        // plugin.json (the archive root or exactly one nested dir)
        let root = find_manifest_root(&unpack).ok_or("archive has no plugin.json")?;
        let text = read_manifest(&root.join("plugin.json"))?;
        let manifest = Manifest::parse(&text, &entry.id)
            .ok_or_else(|| format!("manifest invalid or id != {:?}", entry.id))?;
        if manifest.cmd.is_empty() || !archive_path_is_safe(&manifest.cmd) {
            return Err("marketplace plugin entry command must stay inside its install directory".to_string());
        }

        let dest = plugins_dir.join(&entry.id);
        crate::ensure_user_dir(plugins_dir).map_err(|e| format!("plugins dir: {e}"))?;
        replace_plugin_dir(&root, &dest, &entry.id).map_err(|e| format!("install move: {e}"))?;
        Ok(manifest)
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn fresh_work_dir(temp_dir: &Path, id: &str) -> Result<PathBuf, String> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..32 {
        let path = temp_dir.join(format!("termie-install-{id}-{}-{nonce}-{attempt}", std::process::id()));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("temp dir: {error}")),
        }
    }
    Err("couldn't create a unique plugin install directory".into())
}

/// remove an installed plugin's directory. id is validated so a bad id can't
/// delete outside the plugins dir
pub fn remove(id: &str, plugins_dir: &Path) -> Result<(), String> {
    if !id_is_safe(id) {
        return Err(format!("unsafe plugin id {id:?}"));
    }
    let dest = plugins_dir.join(id);
    std::fs::remove_dir_all(&dest).map_err(|e| format!("remove: {e}"))
}

/// find the directory under `base` that directly contains plugin.json: either
/// `base` itself or a single nested subdirectory (the common archive layout)
fn find_manifest_root(base: &Path) -> Option<PathBuf> {
    if base.join("plugin.json").is_file() {
        return Some(base.to_path_buf());
    }
    let mut subdir = None;
    for e in std::fs::read_dir(base).ok()?.flatten() {
        if e.path().is_dir() {
            if subdir.is_some() {
                return None; // ambiguous: more than one dir
            }
            subdir = Some(e.path());
        }
    }
    let sub = subdir?;
    if sub.join("plugin.json").is_file() {
        Some(sub)
    } else {
        None
    }
}

fn replace_plugin_dir(from: &Path, dest: &Path, id: &str) -> std::io::Result<()> {
    let parent = dest.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "plugin destination has no parent")
    })?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let suffix = format!("{id}-{}-{nonce}", std::process::id());
    let staged = parent.join(format!(".termie-install-{suffix}"));
    let backup = parent.join(format!(".termie-backup-{suffix}"));

    move_dir(from, &staged)?;

    let had_existing = std::fs::symlink_metadata(dest).is_ok();
    if had_existing && let Err(error) = std::fs::rename(dest, &backup) {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(error);
    }

    if let Err(error) = std::fs::rename(&staged, dest) {
        if had_existing && let Err(rollback_error) = std::fs::rename(&backup, dest) {
            return Err(std::io::Error::other(format!(
                "failed to activate plugin: {error}; failed to restore previous plugin: {rollback_error}; backup remains at {}",
                backup.display()
            )));
        }
        let _ = std::fs::remove_dir_all(&staged);
        return Err(error);
    }

    if had_existing {
        let _ = std::fs::remove_dir_all(&backup);
    }
    Ok(())
}

/// move a directory, falling back to recursive copy + delete across volumes
/// (temp and %APPDATA% can be on different drives, where rename fails)
fn move_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if let Err(e) = copy_dir(from, to) {
        let _ = std::fs::remove_dir_all(to);
        return Err(e);
    }
    std::fs::remove_dir_all(from)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    crate::ensure_user_dir(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dst = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dst)?;
        } else {
            std::fs::copy(e.path(), &dst)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn unix_helper_resolution_requires_an_absolute_path() {
        assert_eq!(quiet_command("/bin/sh").get_program(), std::ffi::OsStr::new("/bin/sh"));
        assert!(unix_helper_path("sh", None).is_some());
        assert_eq!(unix_helper_path("termie-helper-that-does-not-exist", None), None);
        assert_eq!(quiet_command("termie-helper-that-does-not-exist").get_program(), std::ffi::OsStr::new(""));
    }

    #[test]
    fn parse_index_extracts_entries() {
        let text = r#"{"plugins":[
            {"id":"pet","name":"Pet","version":"1.0.0","description":"a pet","url":"https://raw.githubusercontent.com/zeo/termie-plugins/main/dist/pet.zip","permissions":["write_pty"]},
            {"id":"relay","name":"Relay","version":"0.1.0","description":"bus","url":"https://raw.githubusercontent.com/zeo/termie-plugins/main/dist/relay.zip"}
        ]}"#;
        let e = parse_index(text);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].id, "pet");
        assert_eq!(e[0].url, catalog_raw_url("dist/pet.zip"));
        assert_eq!(e[0].permissions, vec!["write_pty".to_string()]);
        assert_eq!(e[1].id, "relay");
        assert!(e[1].permissions.is_empty());
    }

    #[test]
    fn catalog_urls_share_the_named_repository_and_ref() {
        assert_eq!(
            catalog_raw_url("index.json"),
            format!("https://raw.githubusercontent.com/zeo/termie-plugins/{CATALOG_REF}/index.json")
        );
        assert_eq!(
            catalog_raw_url("plugins/pet.zip"),
            format!("https://raw.githubusercontent.com/zeo/termie-plugins/{CATALOG_REF}/plugins/pet.zip")
        );
    }

    #[test]
    fn parse_index_drops_unsafe_ids_and_urlless() {
        let text = r#"{"plugins":[
            {"id":"../evil","url":"https://raw.githubusercontent.com/zeo/termie-plugins/main/dist/e.zip"},
            {"id":"ok","url":""},
            {"id":"file","url":"file:///etc/passwd"},
            {"id":"options","url":"--output=/tmp/termie"},
            {"id":"credentials","url":"https://user:pass@raw.githubusercontent.com/zeo/termie-plugins/main/dist/g.zip"},
            {"id":"outside","url":"https://example.com/g.zip"},
            {"id":"good","url":"https://raw.githubusercontent.com/zeo/termie-plugins/main/dist/g.zip"}
        ]}"#;
        let e = parse_index(text);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].id, "good");
    }

    #[test]
    fn marketplace_urls_stay_inside_the_pinned_catalog() {
        for url in [
            "https://raw.githubusercontent.com/zeo/termie-plugins/main/dist/pet.zip",
            concat!(
                "https://raw.githubusercontent.com/zeo/termie-plugins/",
                "6b21244e584bc344d21fdafcc3300264dea58364/dist/pet.zip"
            ),
        ] {
            assert!(marketplace_url_is_safe(url), "{url:?} should be accepted");
        }
        for url in [
            "https://raw.githubusercontent.com/zeo/termie-plugins/other/dist/pet.zip",
            "https://raw.githubusercontent.com/other/termie-plugins/main/dist/pet.zip",
            "https://raw.githubusercontent.com/zeo/termie-plugins/main/dist/pet.txt",
            "https://raw.githubusercontent.com/zeo/termie-plugins/main/../pet.zip",
            "https://example.com/pet.zip",
            "file:///etc/passwd",
            "https://raw.githubusercontent.com/zeo/termie-plugins/main/dist/pet.zip#fragment",
            "https://raw.githubusercontent.com/zeo/termie-plugins/main/dist/pet zip",
            "-o/tmp/termie",
        ] {
            assert!(!marketplace_url_is_safe(url), "{url:?} should be rejected");
        }
    }

    #[test]
    fn marketplace_curl_rejects_non_https_redirects() {
        let args: Vec<_> = marketplace_curl()
            .args(["-fsSL", "--", "https://plugins.example/pet.zip"])
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-q",
                "--globoff",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "-fsSL",
                "--",
                "https://plugins.example/pet.zip"
            ]
        );
    }

    #[test]
    fn archive_paths_cannot_escape_install_dir() {
        assert!(archive_path_is_safe("plugin/plugin.json"));
        assert!(archive_path_is_safe("plugin/assets/icon.png"));
        assert!(!archive_path_is_safe("../plugin.json"));
        assert!(!archive_path_is_safe(r"plugin\..\outside.exe"));
        assert!(!archive_path_is_safe("C:/outside.exe"));
        assert!(!archive_path_is_safe("plugin/../../outside"));
        assert!(!archive_path_is_safe("/tmp/plugin.json"));
    }

    #[test]
    fn archive_metadata_allows_only_files_and_directories() {
        let tar = b"drwxr-xr-x  0 owner group 0 Jan 1 00:00 plugin/\n-rw-r--r--  0 owner group 2 Jan 1 00:00 plugin/plugin.json\n";
        assert!(validate_tar_types(tar.to_vec(), 2).is_ok());
        assert!(validate_tar_types(tar.to_vec(), 1).is_err());
        assert!(validate_tar_types(b"lrwxrwxrwx link -> ..\n".to_vec(), 1).is_err());

        let zip = b"Archive: plugin.zip\ndrwxr-xr-x  3.0 unx 0 bx stor 21-Jul-26 12:00 plugin/\n-rw-r--r--  3.0 unx 2 tx defN 21-Jul-26 12:00 plugin/plugin.json\n2 files, 2 bytes uncompressed\n";
        assert!(validate_zipinfo_types(zip.to_vec(), 2).is_ok());
        assert!(validate_zipinfo_types(zip.to_vec(), 1).is_err());
        let linked = b"lrwxrwxrwx  3.0 unx 2 bx stor 21-Jul-26 12:00 plugin/escape\n";
        assert!(validate_zipinfo_types(linked.to_vec(), 1).is_err());

        let huge_tar = format!("-rw-r--r--  0 owner group {} Jan 1 00:00 plugin/huge", MAX_ARCHIVE_EXTRACTED_BYTES + 1);
        assert!(validate_tar_types(huge_tar.into_bytes(), 1).is_err());
        let huge_zip = format!(
            "-rw-r--r--  3.0 unx {} tx defN 21-Jul-26 12:00 plugin/huge",
            MAX_ARCHIVE_EXTRACTED_BYTES + 1
        );
        assert!(validate_zipinfo_types(huge_zip.into_bytes(), 1).is_err());
    }

    #[test]
    fn bounded_reader_refuses_the_first_byte_over_limit() {
        assert_eq!(read_bounded(std::io::Cursor::new(b"abc"), 3).unwrap(), b"abc");
        assert!(matches!(
            read_bounded(std::io::Cursor::new(b"abcd"), 3),
            Err(BoundedOutputError::Limit)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_output_kills_a_silent_helper_at_its_deadline() {
        let mut command = Command::new("/bin/sleep");
        command.arg("5");
        let started = std::time::Instant::now();
        let result = bounded_output_with_deadline(&mut command, 1024, std::time::Duration::from_millis(50));
        assert!(matches!(result, Err(BoundedOutputError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_output_times_out_after_early_stdout() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf ready; sleep 5"]);
        let result = bounded_output_with_deadline(&mut command, 1024, std::time::Duration::from_millis(50));
        assert!(matches!(result, Err(BoundedOutputError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_output_kills_children_that_hold_its_pipes_open() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 5 & wait"]);
        let started = std::time::Instant::now();
        let result = bounded_output_with_deadline(&mut command, 1024, std::time::Duration::from_millis(50));
        assert!(matches!(result, Err(BoundedOutputError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_output_keeps_completed_stdout() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf ready"]);
        let output = bounded_output_with_deadline(&mut command, 1024, std::time::Duration::from_millis(50)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ready");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn archive_metadata_output_is_bounded() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "head -c 16777217 /dev/zero"]);
        assert!(matches!(
            bounded_archive_metadata(&mut command),
            Err(message) if message == "archive metadata exceeds the 16 MiB limit"
        ));
    }

    #[test]
    fn manifest_reader_rejects_oversized_files() {
        let base = temp_subdir("manifest-size");
        let manifest = base.join("plugin.json");
        std::fs::write(&manifest, vec![b'x'; MAX_MANIFEST_BYTES + 1]).unwrap();
        assert_eq!(
            read_manifest(&manifest),
            Err("manifest exceeds the 64 KiB limit".to_string())
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs zip and unzip installed"]
    fn linux_rejects_a_zip_symlink_before_extraction() {
        let base = temp_subdir("linked");
        let package = base.join("plugin");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("plugin.json"), "{}").unwrap();
        std::os::unix::fs::symlink("..", package.join("escape")).unwrap();
        let status = std::process::Command::new("zip")
            .current_dir(&base)
            .args(["-qry", "plugin.zip", "plugin"])
            .status()
            .unwrap();
        assert!(status.success());
        let unpack = base.join("unpack");
        std::fs::create_dir(&unpack).unwrap();
        assert_eq!(
            unpack_archive(&base.join("plugin.zip"), &unpack),
            Err("archive contains a link or special file".to_string())
        );
        assert!(std::fs::read_dir(&unpack).unwrap().next().is_none());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn parse_index_handles_garbage() {
        assert!(parse_index("not json").is_empty());
        assert!(parse_index("{}").is_empty());
        assert!(parse_index(r#"{"plugins":"nope"}"#).is_empty());
    }

    #[test]
    fn remove_rejects_unsafe_id() {
        let dir = std::env::temp_dir();
        assert!(remove("../etc", &dir).is_err());
        assert!(remove("", &dir).is_err());
    }

    #[test]
    fn plugin_installs_get_distinct_work_directories() {
        let base = temp_subdir("work-dir");
        let first = fresh_work_dir(&base, "pet").expect("first work directory");
        let second = fresh_work_dir(&base, "pet").expect("second work directory");
        assert_ne!(first, second);
        assert!(first.is_dir());
        assert!(second.is_dir());
        std::fs::remove_dir_all(base).unwrap();
    }

    fn temp_subdir(tag: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut d = std::env::temp_dir();
        d.push(format!("termie-mkt-{tag}-{}-{nonce}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn plugin_replacement_preserves_the_previous_install_until_staged() {
        let base = temp_subdir("replacement");
        let dest = base.join("pet");
        let incoming = base.join("incoming");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("version"), "old").unwrap();

        assert!(replace_plugin_dir(&base.join("missing"), &dest, "pet").is_err());
        assert_eq!(std::fs::read_to_string(dest.join("version")).unwrap(), "old");

        std::fs::create_dir_all(&incoming).unwrap();
        std::fs::write(incoming.join("version"), "new").unwrap();
        replace_plugin_dir(&incoming, &dest, "pet").unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("version")).unwrap(), "new");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn find_manifest_root_handles_root_nested_and_ambiguous() {
        // plugin.json directly in base -> base itself is the root
        let base = temp_subdir("root");
        std::fs::write(base.join("plugin.json"), "{}").unwrap();
        assert_eq!(find_manifest_root(&base).as_deref(), Some(base.as_path()));
        let _ = std::fs::remove_dir_all(&base);

        // plugin.json one level down -> that single subdir is the root
        let base = temp_subdir("nested");
        let sub = base.join("inner");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("plugin.json"), "{}").unwrap();
        assert_eq!(find_manifest_root(&base), Some(sub));
        let _ = std::fs::remove_dir_all(&base);

        // two subdirs and no manifest at base -> ambiguous, so None
        let base = temp_subdir("ambiguous");
        std::fs::create_dir_all(base.join("a")).unwrap();
        std::fs::create_dir_all(base.join("b")).unwrap();
        assert_eq!(find_manifest_root(&base), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_deletes_a_safe_plugin_dir() {
        let base = temp_subdir("remove");
        std::fs::create_dir_all(base.join("pet")).unwrap();
        std::fs::write(base.join("pet").join("plugin.json"), "{}").unwrap();
        assert!(base.join("pet").exists());
        remove("pet", &base).expect("removing a real plugin dir succeeds");
        assert!(!base.join("pet").exists());
        // removing what's already gone errors, and unsafe ids stay rejected
        assert!(remove("pet", &base).is_err());
        assert!(remove("../escape", &base).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(not(windows))]
    #[test]
    #[ignore = "needs zip and unzip installed"]
    fn linux_unpacks_a_real_zip_archive() {
        let base = temp_subdir("unzip");
        let source = base.join("source");
        let unpack = base.join("unpack");
        let archive = base.join("plugin.zip");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&unpack).unwrap();
        std::fs::write(source.join("plugin.json"), r#"{"id":"pet","entry":"pet"}"#).unwrap();
        let status = quiet_command("zip")
            .args(["-q", "-r"])
            .arg(&archive)
            .arg(".")
            .current_dir(&source)
            .status()
            .expect("zip installed");
        assert!(status.success());
        unpack_archive(&archive, &unpack).expect("unzip archive");
        reject_symlinks(&unpack).expect("ordinary archive has no symlinks");
        assert!(unpack.join("plugin.json").is_file());
        std::fs::remove_dir_all(base).unwrap();
    }
}
