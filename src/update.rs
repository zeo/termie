//! in-app update check + verified native update. once a day (or on request)
//! termie asks GitHub for the latest release; a newer version puts an UPDATE
//! chip on the status bar. confirmed Windows and archive-managed Linux installs
//! replace themselves and restart. opt out with `update_check=false`

use std::path::PathBuf;

use crate::plugin::json::Json;
use crate::plugin::market::{bounded_output, quiet_command, BoundedOutputError};

const PROJECT_REPOSITORY: &str = "https://github.com/zeo/termie";
const RELEASES_URL: &str = "https://api.github.com/repos/zeo/termie/releases/latest";
const MAX_RELEASE_METADATA_BYTES: usize = 1024 * 1024;
const MAX_RELEASE_ASSET_BYTES: usize = 128 * 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_UPDATE_ARCHIVE_LISTING_BYTES: usize = 16 * 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_UPDATE_ARCHIVE_ENTRIES: usize = 4096;
#[cfg(target_os = "linux")]
const MAX_UPDATE_ARCHIVE_EXTRACT_OUTPUT_BYTES: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const MAX_UPDATE_INSTALLER_OUTPUT_BYTES: usize = 1024 * 1024;

fn update_curl() -> std::process::Command {
    let mut command = quiet_command("curl");
    // -q must stay first so a user's curlrc cannot relax the updater's policy
    command.args(["-q", "--globoff", "--proto", "=https", "--proto-redir", "=https"]);
    command
}

#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub version: String,
    pub url: String,
    /// sha256 hex of the release asset, from the release api; the download is
    /// refused unless it hashes to exactly this
    pub digest: String,
}

#[cfg(windows)]
pub fn can_install() -> bool {
    true
}

#[cfg(target_os = "linux")]
pub fn archive_install_prefix() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let bin = exe.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let prefix = bin.parent()?.to_path_buf();
    prefix.join("share/termie/archive-install").is_file().then_some(prefix)
}

#[cfg(target_os = "linux")]
pub fn can_install() -> bool {
    archive_install_prefix().is_some()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn can_install() -> bool {
    false
}

fn stamp_path() -> Option<PathBuf> {
    Some(crate::cache_dir()?.join("update.stamp"))
}

/// at most one automatic check per ~20h (a manual palette check skips this)
pub fn due() -> bool {
    let Some(p) = stamp_path() else {
        return false;
    };
    match std::fs::metadata(&p).and_then(|m| m.modified()) {
        Ok(t) => t.elapsed().map(|e| e.as_secs() > 20 * 60 * 60).unwrap_or(true),
        Err(_) => true,
    }
}

pub fn mark_checked() {
    if let Some(p) = stamp_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, b"checked");
    }
}

/// query the latest release off-thread; None = no newer version (or any failure
/// — an update check must never surface an error on its own)
pub fn check(on_done: impl FnOnce(Option<Update>) + Send + 'static) {
    std::thread::spawn(move || {
        on_done(fetch_latest().filter(|u| newer_than_current(&u.version)));
    });
}

fn fetch_latest() -> Option<Update> {
    let mut command = update_curl();
    command.args(["-s", "-L", "--max-time", "20", "--", RELEASES_URL]);
    let out = bounded_output(&mut command, MAX_RELEASE_METADATA_BYTES).ok()?;
    if !out.status.success() {
        return None;
    }
    parse_release(&String::from_utf8_lossy(&out.stdout))
}

fn bounded_output_error(error: BoundedOutputError, limit: usize) -> String {
    match error {
        BoundedOutputError::Io(error) => error.to_string(),
        BoundedOutputError::Limit => format!("download exceeds the {} MiB limit", limit / 1024 / 1024),
    }
}

fn asset_name(version: &str) -> Option<String> {
    #[cfg(windows)]
    return Some(format!("termie-{version}-setup.exe"));
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return Some(format!("termie-{version}-linux-x86_64.tar.gz"));
    #[allow(unreachable_code)]
    None
}

fn release_asset_url_is_safe(version: &str, name: &str, url: &str) -> bool {
    url == format!("{PROJECT_REPOSITORY}/releases/download/v{version}/{name}")
}

pub fn release_page_url(version: &str) -> String {
    format!("{PROJECT_REPOSITORY}/releases/tag/v{version}")
}

fn parse_release(text: &str) -> Option<Update> {
    let json = Json::parse(text)?;
    let tag = json.get_str("tag_name")?;
    let version = tag.trim_start_matches('v').to_string();
    parse_triple(&version)?;
    let name = asset_name(&version)?;
    let assets = json.get("assets")?.as_array()?;
    let (url, digest) = assets.iter().find_map(|a| {
        if a.get_str("name")? != name {
            return None;
        }
        let url = a.get_str("browser_download_url")?;
        if !release_asset_url_is_safe(&version, &name, url) {
            return None;
        }
        let digest = a.get_str("digest")?.strip_prefix("sha256:")?;
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some((url.to_string(), digest.to_ascii_lowercase()))
    })?;
    Some(Update { version, url, digest })
}

/// strict x.y.z compare against the running build; pre-release tags and
/// unparsable versions never count as updates
pub fn newer_than_current(remote: &str) -> bool {
    newer(remote, env!("CARGO_PKG_VERSION"))
}

fn parse_triple(v: &str) -> Option<(u64, u64, u64)> {
    if v.contains('-') {
        return None; // never offer an rc/pre-release
    }
    let mut it = v.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next()?.parse().ok()?;
    it.next().is_none().then_some((a, b, c))
}

fn newer(remote: &str, local: &str) -> bool {
    match (parse_triple(remote), parse_triple(local)) {
        (Some(r), Some(l)) => r > l,
        _ => false,
    }
}

fn fresh_temp_dir() -> Result<PathBuf, String> {
    let base = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    for attempt in 0..32 {
        let dir = base.join(format!("termie-update-{}-{nonce}-{attempt}", std::process::id()));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("couldn't create update directory".into())
}

/// download and verify the native release asset; blocking — run on a worker thread
pub fn download(u: &Update) -> Result<PathBuf, String> {
    #[cfg(target_os = "linux")]
    let prefix = archive_install_prefix();
    #[cfg(not(target_os = "linux"))]
    let prefix: Option<PathBuf> = None;
    download_to_prefix(u, prefix.as_deref())
}

fn download_to_prefix(u: &Update, prefix: Option<&std::path::Path>) -> Result<PathBuf, String> {
    #[cfg(not(target_os = "linux"))]
    let _ = prefix;
    let name = asset_name(&u.version).ok_or("updates aren't available for this platform")?;
    if !release_asset_url_is_safe(&u.version, &name, &u.url) {
        return Err("update URL does not match the release asset".into());
    }
    let dir = fresh_temp_dir()?;
    let path = dir.join(name);
    let fetched = (|| {
        let mut command = update_curl();
        command.args(["-fsSL", "--max-time", "300", "--", &u.url]);
        let out = bounded_output(&mut command, MAX_RELEASE_ASSET_BYTES)
            .map_err(|error| bounded_output_error(error, MAX_RELEASE_ASSET_BYTES))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        verify_download(&out.stdout, &u.digest)?;
        std::fs::write(&path, &out.stdout).map_err(|e| e.to_string())
    })();
    if let Err(e) = fetched {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    #[cfg(windows)]
    return Ok(path);
    #[cfg(target_os = "linux")]
    {
        let installed = prefix
            .ok_or_else(|| "this installation is managed outside termie".to_string())
            .and_then(|prefix| install_linux_archive(&path, u, prefix));
        let _ = std::fs::remove_dir_all(&dir);
        return installed;
    }
    #[allow(unreachable_code)]
    Err("updates aren't available for this platform".into())
}

fn verify_download(bytes: &[u8], digest: &str) -> Result<(), String> {
    if bytes.len() < 1024 * 1024 {
        return Err("download incomplete".into());
    }
    (sha256_hex(bytes) == digest).then_some(()).ok_or_else(|| "checksum mismatch".into())
}

#[cfg(target_os = "linux")]
fn archive_path_is_safe(path: &str, root: &str) -> bool {
    let mut parts = std::path::Path::new(path).components();
    matches!(parts.next(), Some(std::path::Component::Normal(first)) if first == root)
        && parts.all(|part| matches!(part, std::path::Component::Normal(_) | std::path::Component::CurDir))
}

#[cfg(target_os = "linux")]
fn validate_update_archive_listing(bytes: Vec<u8>, root: &str) -> Result<usize, String> {
    let listing = String::from_utf8(bytes).map_err(|_| "archive contains non-UTF-8 paths")?;
    let mut entries = 0;
    for path in listing.lines() {
        if !archive_path_is_safe(path, root) {
            return Err("archive contains an unsafe path".into());
        }
        entries += 1;
        if entries > MAX_UPDATE_ARCHIVE_ENTRIES {
            return Err("archive contains too many entries".into());
        }
    }
    (entries > 0).then_some(entries).ok_or_else(|| "archive is empty".into())
}

#[cfg(target_os = "linux")]
fn validate_update_archive_types(bytes: Vec<u8>, expected: usize) -> Result<(), String> {
    let listing = String::from_utf8(bytes).map_err(|_| "archive contains non-UTF-8 metadata")?;
    let mut entries = 0;
    for line in listing.lines() {
        if line.is_empty() {
            continue;
        }
        entries += 1;
        if !matches!(line.as_bytes().first(), Some(b'-' | b'd')) {
            return Err("archive contains a link or special file".into());
        }
    }
    (entries == expected)
        .then_some(())
        .ok_or_else(|| "archive metadata does not match its file list".into())
}

#[cfg(target_os = "linux")]
fn reject_symlinks(dir: &std::path::Path) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            return Err("archive contains a symlink".into());
        }
        if kind.is_dir() {
            reject_symlinks(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_linux_archive(
    archive: &std::path::Path,
    u: &Update,
    prefix: &std::path::Path,
) -> Result<PathBuf, String> {
    let root = format!("termie-{}-linux-x86_64", u.version);
    let mut listing = quiet_command("/bin/tar");
    listing.args(["-tzf"]).arg(archive);
    let listing = bounded_output(&mut listing, MAX_UPDATE_ARCHIVE_LISTING_BYTES).map_err(|error| match error {
        BoundedOutputError::Io(error) => format!("couldn't inspect update: {error}"),
        BoundedOutputError::Limit => "update archive metadata exceeds the 16 MiB limit".to_string(),
    })?;
    if !listing.status.success() {
        return Err("couldn't inspect update archive".into());
    }
    let entries = validate_update_archive_listing(listing.stdout, &root)?;
    let mut kinds = quiet_command("/bin/tar");
    kinds.args(["-tvzf"]).arg(archive);
    let kinds = bounded_output(&mut kinds, MAX_UPDATE_ARCHIVE_LISTING_BYTES).map_err(|error| match error {
        BoundedOutputError::Io(error) => format!("couldn't inspect update types: {error}"),
        BoundedOutputError::Limit => "update archive metadata exceeds the 16 MiB limit".to_string(),
    })?;
    if !kinds.status.success() {
        return Err("archive contains a link or special file".into());
    }
    validate_update_archive_types(kinds.stdout, entries)?;
    let dir = archive.parent().ok_or("invalid update path")?;
    let mut extract = quiet_command("/bin/tar");
    extract
        .args(["--no-same-owner", "--no-same-permissions", "-xzf"])
        .arg(archive)
        .arg("-C")
        .arg(dir);
    let output = bounded_output(&mut extract, MAX_UPDATE_ARCHIVE_EXTRACT_OUTPUT_BYTES).map_err(|error| match error {
        BoundedOutputError::Io(error) => format!("couldn't unpack update: {error}"),
        BoundedOutputError::Limit => "update archive extraction output exceeds the 64 KiB limit".to_string(),
    })?;
    if !output.status.success() {
        return Err("couldn't unpack update".into());
    }
    let unpacked = dir.join(root);
    reject_symlinks(&unpacked)?;
    let installer = unpacked.join("install.sh");
    if !installer.is_file() {
        return Err("update archive has no installer".into());
    }
    let mut installer_command = quiet_command("/bin/sh");
    installer_command.arg(installer).arg(prefix);
    let out = bounded_output(&mut installer_command, MAX_UPDATE_INSTALLER_OUTPUT_BYTES).map_err(|error| match error {
        BoundedOutputError::Io(error) => format!("couldn't run update installer: {error}"),
        BoundedOutputError::Limit => "update installer output exceeds the 1 MiB limit".to_string(),
    })?;
    if !out.status.success() {
        let reason = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if reason.is_empty() { "update installer failed".into() } else { reason });
    }
    let exe = prefix.join("bin/termie");
    exe.is_file().then_some(exe).ok_or_else(|| "updated executable is missing".into())
}

/// fips 180-4 sha-256, hand-rolled like the rest of termie's codecs; the
/// updater must not run a downloaded installer on file size alone
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bitlen = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, c) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *s = s.wrapping_add(v);
        }
    }
    use std::fmt::Write;
    h.iter().fold(String::with_capacity(64), |mut out, v| {
        let _ = write!(out, "{v:08x}");
        out
    })
}

/// hand off to the installer's silent update mode; the caller exits right after
#[cfg(windows)]
pub fn run_setup(path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new(path)
        .arg("/update")
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn run_setup(_path: &std::path::Path) -> Result<(), String> {
    Err("updates aren't available for this platform".into())
}

#[cfg(target_os = "linux")]
fn spawn_after(parent: u32, path: &std::path::Path) -> Result<std::process::Child, String> {
    let parent = parent.to_string();
    std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "while kill -0 \"$1\" 2>/dev/null; do sleep 1; done; exec \"$2\"",
            "termie-update",
            &parent,
        ])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "linux")]
pub fn run_setup(path: &std::path::Path) -> Result<(), String> {
    spawn_after(std::process::id(), path).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(newer("0.3.1", "0.3.0"));
        assert!(newer("1.0.0", "0.9.9"));
        assert!(newer("0.10.0", "0.9.0"));
        assert!(!newer("0.3.0", "0.3.0"));
        assert!(!newer("0.2.9", "0.3.0"));
        // pre-releases and garbage never count
        assert!(!newer("0.4.0-rc1", "0.3.0"));
        assert!(!newer("banana", "0.3.0"));
        assert!(!newer("0.4", "0.3.0"));
    }

    #[test]
    fn release_json_parses() {
        let name = asset_name("9.9.9").unwrap();
        let digest = "AB".repeat(32);
        let text = format!(
            r#"{{"tag_name":"v9.9.9","assets":[{{"name":"{name}","browser_download_url":"https://github.com/zeo/termie/releases/download/v9.9.9/{name}","digest":"sha256:{digest}"}}]}}"#
        );
        let update = parse_release(&text).unwrap();
        assert_eq!(update.version, "9.9.9");
        assert_eq!(update.digest, digest.to_ascii_lowercase());
        assert!(update.url.ends_with(&name));
        assert!(parse_release(&text.replace(&digest, "AB12")).is_none());
        assert!(parse_release(&text.replace("github.com/zeo", "example.com/zeo")).is_none());
    }

    #[test]
    fn updater_curl_ignores_user_config_and_accepts_https_only() {
        let args: Vec<_> = update_curl()
            .args(["-fsSL", "--", RELEASES_URL])
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["-q", "--globoff", "--proto", "=https", "--proto-redir", "=https", "-fsSL", "--", RELEASES_URL]
        );
    }

    #[test]
    fn release_assets_use_the_exact_official_download_url() {
        let name = asset_name("1.2.3").unwrap();
        let url = format!("{PROJECT_REPOSITORY}/releases/download/v1.2.3/{name}");
        assert!(release_asset_url_is_safe("1.2.3", &name, &url));
        assert!(!release_asset_url_is_safe("1.2.3", &name, "https://example.com/termie.zip"));
        assert!(!release_asset_url_is_safe("1.2.3", &name, &format!("{url}?download=1")));
        assert!(!release_asset_url_is_safe("1.2.3", &name, &format!("{url}#asset")));
    }

    #[test]
    fn release_page_uses_the_project_repository() {
        assert_eq!(release_page_url("1.2.3"), "https://github.com/zeo/termie/releases/tag/v1.2.3");
    }

    #[test]
    fn download_verification_rejects_short_and_wrong_payloads() {
        assert_eq!(verify_download(b"error page", &sha256_hex(b"error page")), Err("download incomplete".into()));
        let bytes = vec![0x5a; 1024 * 1024];
        assert_eq!(verify_download(&bytes, &"00".repeat(32)), Err("checksum mismatch".into()));
        assert_eq!(verify_download(&bytes, &sha256_hex(&bytes)), Ok(()));
    }

    #[test]
    fn bounded_release_downloads_report_the_limit() {
        assert_eq!(
            bounded_output_error(BoundedOutputError::Limit, MAX_RELEASE_ASSET_BYTES),
            "download exceeds the 128 MiB limit"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_archive_paths_stay_under_the_release_root() {
        let root = "termie-1.2.3-linux-x86_64";
        assert!(archive_path_is_safe("termie-1.2.3-linux-x86_64/bin/termie", root));
        assert!(archive_path_is_safe("termie-1.2.3-linux-x86_64/", root));
        assert!(!archive_path_is_safe("../termie/bin/termie", root));
        assert!(!archive_path_is_safe("termie-1.2.3-linux-x86_64/../outside", root));
        assert!(!archive_path_is_safe("other/bin/termie", root));
        assert!(!archive_path_is_safe("/termie-1.2.3-linux-x86_64/bin/termie", root));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_archive_listing_limits_entries_and_matches_types() {
        let root = "termie-1.2.3-linux-x86_64";
        let listing = format!("{root}/bin/termie\n{root}/install.sh\n");
        assert_eq!(validate_update_archive_listing(listing.into_bytes(), root), Ok(2));
        assert!(validate_update_archive_types(b"- file\n- file\n".to_vec(), 2).is_ok());
        assert!(validate_update_archive_types(b"- file\n".to_vec(), 2).is_err());

        let oversized = std::iter::repeat_n(format!("{root}/file\n"), MAX_UPDATE_ARCHIVE_ENTRIES + 1)
            .collect::<String>();
        assert_eq!(
            validate_update_archive_listing(oversized.into_bytes(), root),
            Err("archive contains too many entries".into())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_archive_rejects_links_before_extraction() {
        let work = fresh_temp_dir().unwrap();
        let root = "termie-9.9.9-linux-x86_64";
        let package = work.join("build").join(root);
        std::fs::create_dir_all(&package).unwrap();
        std::os::unix::fs::symlink("/tmp", package.join("escape")).unwrap();
        let archive = work.join("linked.tar.gz");
        let status = std::process::Command::new("tar")
            .args(["-C", &work.join("build").to_string_lossy(), "-czf", &archive.to_string_lossy(), root])
            .status()
            .unwrap();
        assert!(status.success());
        let update = Update {
            version: "9.9.9".into(),
            url: String::new(),
            digest: String::new(),
        };
        assert_eq!(
            install_linux_archive(&archive, &update, &work.join("prefix")),
            Err("archive contains a link or special file".into())
        );
        assert!(!work.join(root).exists());
        let _ = std::fs::remove_dir_all(work);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_archive_install_verifies_and_installs() {
        let work = fresh_temp_dir().unwrap();
        let root = "termie-9.9.9-linux-x86_64";
        let package = work.join("build").join(root);
        std::fs::create_dir_all(package.join("bin")).unwrap();
        let mut state = 0x9e37_79b9_u32;
        let payload: Vec<u8> = (0..1_100_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        std::fs::write(package.join("bin/termie"), &payload).unwrap();
        std::fs::write(
            package.join("install.sh"),
            b"#!/bin/sh\nset -eu\nroot=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\ninstall -d \"$1/bin\" \"$1/share/termie\"\ninstall -m755 \"$root/bin/termie\" \"$1/bin/termie\"\ninstall -m644 /dev/null \"$1/share/termie/archive-install\"\n",
        )
        .unwrap();
        let archive = work.join("update.tar.gz");
        let status = std::process::Command::new("tar")
            .args(["-C", &work.join("build").to_string_lossy(), "-czf", &archive.to_string_lossy(), root])
            .status()
            .unwrap();
        assert!(status.success());
        let prefix = work.join("prefix");
        let update = Update {
            version: "9.9.9".into(),
            url: String::new(),
            digest: String::new(),
        };
        let exe = install_linux_archive(&archive, &update, &prefix).unwrap();
        assert_eq!(std::fs::read(exe).unwrap(), payload);
        assert!(prefix.join("share/termie/archive-install").is_file());
        let _ = std::fs::remove_dir_all(work);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_relaunch_waits_for_the_old_process() {
        use std::os::unix::fs::PermissionsExt;
        let work = fresh_temp_dir().unwrap();
        let marker = work.join("started");
        let target = work.join("new-termie");
        std::fs::write(&target, format!("#!/bin/sh\nprintf started > '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut old = std::process::Command::new("/bin/sh").args(["-c", "sleep 0.2"]).spawn().unwrap();
        let mut relaunch = spawn_after(old.id(), &target).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!marker.exists());
        assert!(old.wait().unwrap().success());
        assert!(relaunch.wait().unwrap().success());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "started");
        let _ = std::fs::remove_dir_all(work);
    }

    #[test]
    fn sha256_matches_the_fips_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // multi-block input (padding lands in a second block)
        assert_eq!(
            sha256_hex(&[b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }
}
