//! plugin marketplace: a curated remote index of installable plugins plus the
//! local install/remove plumbing. archive work shells out to `tar` (ships with
//! Windows). catalog + plugin downloads go through the GitHub CLI (`gh`) when
//! the file lives in the catalog repo, so a private catalog works with the
//! user's existing login, and fall back to anonymous `curl` for a public host.
//!
//! security: downloaded archives are unpacked into a fresh temp dir and the
//! resulting `plugin.json` is validated (id safe + matches the catalog id)
//! BEFORE anything is moved into the plugins directory, so a malicious archive
//! can't traverse out or shadow another plugin.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::json::Json;
use super::manifest::{id_is_safe, Manifest};

/// the marketplace index URL. a plain JSON catalog (see `Entry`); curated, since
/// the security model is trust-the-store (subprocess is not a sandbox)
pub const INDEX_URL: &str =
    "https://raw.githubusercontent.com/lintowe/termie-plugins/main/index.json";
/// the catalog repo + ref behind the raw URLs above. files under this prefix are
/// fetched through `gh` (authenticated) so a private catalog works; everything
/// else falls back to anonymous curl
const CATALOG_REPO: &str = "lintowe/termie-plugins";
const CATALOG_REF: &str = "main";
const CATALOG_RAW_PREFIX: &str = "https://raw.githubusercontent.com/lintowe/termie-plugins/main/";

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
            if !id_is_safe(&id) {
                return None;
            }
            Some(Entry {
                id,
                name: e.get_str("name").unwrap_or("").to_string(),
                version: e.get_str("version").unwrap_or("0.0.0").to_string(),
                description: e.get_str("description").unwrap_or("").to_string(),
                url: e.get_str("url").unwrap_or("").to_string(),
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

/// fetch raw bytes for a catalog URL. files under the catalog repo go through
/// the GitHub CLI (`gh api … Accept: raw`) so a private repo works with the
/// user's login; anything else — or a missing/unauthenticated gh — falls back
/// to anonymous curl
fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    let repo_path = url.strip_prefix(CATALOG_RAW_PREFIX);
    if let Some(path) = repo_path {
        let api = format!("repos/{CATALOG_REPO}/contents/{path}?ref={CATALOG_REF}");
        match Command::new("gh")
            .args(["api", &api, "-H", "Accept: application/vnd.github.raw"])
            .output()
        {
            Ok(o) if o.status.success() => return Ok(o.stdout),
            Ok(o) => log::warn!("gh fetch of {path} failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => log::warn!("gh unavailable ({e}); trying curl"),
        }
    }
    match Command::new("curl").args(["-fsSL", "--max-time", "60", url]).output() {
        Ok(o) if o.status.success() => Ok(o.stdout),
        Ok(_) if repo_path.is_some() => {
            Err("couldn't reach the catalog — install the GitHub CLI and run `gh auth login`".to_string())
        }
        Ok(o) => Err(format!("fetch failed (curl exit {})", o.status.code().unwrap_or(-1))),
        Err(e) => Err(format!("couldn't run curl: {e}")),
    }
}

/// fetch + parse the catalog index. Ok(entries) on a successful fetch (possibly
/// empty if the catalog is), Err(reason) if the request itself failed — so the
/// store can tell "empty" from "unreachable"
pub fn fetch_index() -> Result<Vec<Entry>, String> {
    let bytes = fetch_bytes(INDEX_URL)?;
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
    let work = temp_dir.join(format!("termie-install-{}", entry.id));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("temp dir: {e}"))?;
    let archive = work.join("plugin.zip");
    let unpack = work.join("unpack");
    std::fs::create_dir_all(&unpack).map_err(|e| format!("unpack dir: {e}"))?;

    // download (authenticated via gh for the private catalog repo, else curl)
    let bytes = fetch_bytes(&entry.url).map_err(|e| format!("download failed: {e}"))?;
    std::fs::write(&archive, &bytes).map_err(|e| format!("write archive: {e}"))?;

    // unpack with tar (handles .zip on modern Windows). -C extracts into unpack
    let ex = Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&unpack)
        .status();
    match ex {
        Ok(s) if s.success() => {}
        Ok(s) => return Err(format!("unpack failed: status {s:?}")),
        Err(e) => return Err(format!("unpack failed: {e}")),
    }

    // the archive may wrap its files in a top dir; find the dir containing
    // plugin.json (the archive root or exactly one nested dir)
    let root = find_manifest_root(&unpack).ok_or("archive has no plugin.json")?;
    let text = std::fs::read_to_string(root.join("plugin.json")).map_err(|e| format!("manifest: {e}"))?;
    let manifest = Manifest::parse(&text, &entry.id)
        .ok_or_else(|| format!("manifest invalid or id != {:?}", entry.id))?;

    // swap into place atomically-ish: remove any existing install, then move
    let dest = plugins_dir.join(&entry.id);
    std::fs::create_dir_all(plugins_dir).map_err(|e| format!("plugins dir: {e}"))?;
    let _ = std::fs::remove_dir_all(&dest);
    move_dir(&root, &dest).map_err(|e| format!("install move: {e}"))?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(manifest)
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

/// move a directory, falling back to recursive copy + delete across volumes
/// (temp and %APPDATA% can be on different drives, where rename fails)
fn move_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_dir(from, to)?;
    std::fs::remove_dir_all(from)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
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

    #[test]
    fn parse_index_extracts_entries() {
        let text = r#"{"plugins":[
            {"id":"pet","name":"Pet","version":"1.0.0","description":"a pet","url":"https://x/pet.zip","permissions":["write_pty"]},
            {"id":"relay","name":"Relay","version":"0.1.0","description":"bus","url":"https://x/relay.zip"}
        ]}"#;
        let e = parse_index(text);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].id, "pet");
        assert_eq!(e[0].url, "https://x/pet.zip");
        assert_eq!(e[0].permissions, vec!["write_pty".to_string()]);
        assert_eq!(e[1].id, "relay");
        assert!(e[1].permissions.is_empty());
    }

    #[test]
    fn parse_index_drops_unsafe_ids_and_urlless() {
        let text = r#"{"plugins":[
            {"id":"../evil","url":"https://x/e.zip"},
            {"id":"ok","url":""},
            {"id":"good","url":"https://x/g.zip"}
        ]}"#;
        let e = parse_index(text);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].id, "good");
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
}
