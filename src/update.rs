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
    pub url: String,
}

fn stamp_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(base).join("termie").join("update.stamp"))
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
    let assets = json.get("assets")?.as_array()?;
    let url = assets.iter().find_map(|a| {
        let name = a.get_str("name")?;
        if name.ends_with("-setup.exe") {
            a.get_str("browser_download_url").map(str::to_string)
        } else {
            None
        }
    })?;
    Some(Update { version, url })
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
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if size < 1024 * 1024 {
        return Err("download incomplete".into());
    }
    Ok(path)
}

/// hand off to the installer's silent update mode; the caller exits right after
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
                {"name": "termie-9.9.9-windows-x64.msi", "browser_download_url": "https://x/msi"},
                {"name": "termie-9.9.9-setup.exe", "browser_download_url": "https://x/setup"}
            ]
        }"#;
        let json = Json::parse(text).unwrap();
        let tag = json.get_str("tag_name").unwrap();
        assert_eq!(tag, "v9.9.9");
        let assets = json.get("assets").unwrap().as_array().unwrap();
        let url = assets
            .iter()
            .find_map(|a| {
                let name = a.get_str("name")?;
                name.ends_with("-setup.exe")
                    .then(|| a.get_str("browser_download_url").map(str::to_string))
                    .flatten()
            })
            .unwrap();
        assert_eq!(url, "https://x/setup");
    }
}
