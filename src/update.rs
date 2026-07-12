//! in-app update check + download. once a day (or on the palette's "install
//! update") termie asks GitHub for the latest release; a newer version puts an
//! UPDATE chip on the status bar. nothing downloads or installs until the user
//! confirms — then the native setup runs with /update and termie restarts into
//! the new build with its session restored. opt out with `update_check=false`

use std::path::PathBuf;

use crate::plugin::json::Json;
use crate::plugin::market::quiet_command;

const RELEASES_URL: &str = "https://api.github.com/repos/lintowe/termie/releases/latest";

#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub version: String,
    #[cfg(windows)]
    pub url: String,
    /// sha256 hex of the setup asset, from the release api; the download is
    /// refused unless it hashes to exactly this
    #[cfg(windows)]
    pub digest: String,
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
    let out = quiet_command("curl")
        .args(["-s", "-L", "--max-time", "20", RELEASES_URL])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let json = Json::parse(&text)?;
    let tag = json.get_str("tag_name")?;
    let version = tag.trim_start_matches('v').to_string();
    // on linux the app never installs anything itself (the chip links to the
    // release page), so a newer tag alone is enough — no asset to verify
    #[cfg(not(windows))]
    {
        Some(Update { version })
    }
    #[cfg(windows)]
    {
        let assets = json.get("assets")?.as_array()?;
        let (url, digest) = assets.iter().find_map(|a| {
            let name = a.get_str("name")?;
            if !name.ends_with("-setup.exe") {
                return None;
            }
            // the installer only ever ships from this repo's release downloads;
            // an api response pointing anywhere else is treated as tampering,
            // and one without a digest can't be verified so it doesn't count
            let url = a
                .get_str("browser_download_url")
                .filter(|u| u.starts_with("https://github.com/lintowe/termie/releases/download/"))?;
            let digest = a.get_str("digest")?.strip_prefix("sha256:")?;
            Some((url.to_string(), digest.to_ascii_lowercase()))
        })?;
        Some(Update { version, url, digest })
    }
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

/// download the setup exe to %TEMP%; blocking — run on a worker thread
#[cfg(windows)]
pub fn download(u: &Update) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("termie-{}-setup.exe", u.version));
    let out = quiet_command("curl")
        .args([
            "-sSL",
            "--max-time",
            "300",
            "-o",
            &path.to_string_lossy(),
            &u.url,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    // a payload-bearing setup is megabytes; a tiny file is an error page
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    if bytes.len() < 1024 * 1024 {
        return Err("download incomplete".into());
    }
    // nothing runs unless it hashes to what the release api published
    if sha256_hex(&bytes) != u.digest {
        let _ = std::fs::remove_file(&path);
        return Err("checksum mismatch".into());
    }
    Ok(path)
}

/// fips 180-4 sha-256, hand-rolled like the rest of termie's codecs; the
/// updater must not run a downloaded installer on file size alone
#[cfg(any(windows, test))]
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
pub fn run_setup(path: &PathBuf) -> Result<(), String> {
    std::process::Command::new(path)
        .arg("/update")
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
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
        let text = r#"{
            "tag_name": "v9.9.9",
            "assets": [
                {"name": "termie-9.9.9-windows-x64.msi", "browser_download_url": "https://github.com/lintowe/termie/releases/download/v9.9.9/termie-9.9.9-windows-x64.msi", "digest": "sha256:aa"},
                {"name": "termie-9.9.9-setup.exe", "browser_download_url": "https://github.com/lintowe/termie/releases/download/v9.9.9/termie-9.9.9-setup.exe", "digest": "sha256:AB12"}
            ]
        }"#;
        let json = Json::parse(text).unwrap();
        assert_eq!(json.get_str("tag_name").unwrap(), "v9.9.9");
        let assets = json.get("assets").unwrap().as_array().unwrap();
        let setup = assets.iter().find(|a| a.get_str("name").unwrap().ends_with("-setup.exe")).unwrap();
        assert!(setup.get_str("browser_download_url").unwrap().contains("/releases/download/"));
        // digest is normalized to bare lowercase hex the way fetch_latest does
        let digest = setup.get_str("digest").unwrap().strip_prefix("sha256:").unwrap().to_ascii_lowercase();
        assert_eq!(digest, "ab12");
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
