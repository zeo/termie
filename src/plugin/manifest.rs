//! plugin manifest parsing + validation. centralizes the `plugin.json` schema
//! and — critically for a marketplace that installs untrusted code — enforces
//! that a plugin id is a safe single path segment so it can never escape the
//! plugins directory (no `..`, no separators, no absolute paths)

use super::json::Json;
use std::path::{Component, Path};

/// a validated plugin manifest. construct via `parse`, which rejects anything
/// unsafe or malformed rather than producing a half-valid value
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: u32,
    /// entry command as written in the manifest (resolved against the plugin
    /// dir by the host; relative is expected for marketplace installs)
    pub cmd: String,
    pub args: Vec<String>,
    pub permissions: Vec<String>,
}

/// a plugin id is safe iff it's a single path segment of a conservative
/// character set: ascii alphanumerics plus `-` `_` `.`, never empty, never `..`,
/// and capped in length. this is the security boundary for install paths
pub fn id_is_safe(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// the permission strings the host understands. unknown permissions in a
/// manifest are dropped (not an error) so future permissions degrade gracefully
pub const KNOWN_PERMISSIONS: &[&str] = &["read_output", "write_pty", "network"];

fn entry_command_is_safe(cmd: &str) -> bool {
    let path = Path::new(cmd);
    if path.is_absolute() {
        return true;
    }
    let mut has_name = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_name = true,
            Component::CurDir => {}
            _ => return false,
        }
    }
    has_name
}

impl Manifest {
    /// parse + validate a manifest from `plugin.json` text. `dir_name` is the
    /// containing directory's name, used as the id fallback and cross-checked
    /// against an explicit id so a manifest can't claim a different identity
    /// than where it lives. returns None (with a logged reason) on any problem
    pub fn parse(text: &str, dir_name: &str) -> Option<Manifest> {
        let json = Json::parse(text)?;

        let id = json.get_str("id").unwrap_or(dir_name).to_string();
        if !id_is_safe(&id) {
            log::warn!("plugin id {id:?} is unsafe; skipping");
            return None;
        }
        // a manifest living in dir X may only claim id X (prevents one install
        // from shadowing another by id)
        if id != dir_name {
            log::warn!("plugin id {id:?} does not match its directory {dir_name:?}; skipping");
            return None;
        }

        let entry = json.get("entry")?;
        let cmd = entry.get_str("cmd")?.to_string();
        if !entry_command_is_safe(&cmd) {
            return None;
        }
        let args = entry
            .get("args")
            .and_then(Json::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();

        let api_version = json.get("api_version").and_then(Json::as_f64).unwrap_or(0.0) as u32;
        let permissions = json
            .get("permissions")
            .and_then(Json::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|p| KNOWN_PERMISSIONS.contains(p))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        Some(Manifest {
            id,
            name: json.get_str("name").unwrap_or(dir_name).to_string(),
            version: json.get_str("version").unwrap_or("0.0.0").to_string(),
            api_version,
            cmd,
            args,
            permissions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_ids() {
        for bad in ["", "..", ".", "a/b", "a\\b", "../evil", "a b", "x".repeat(65).as_str()] {
            assert!(!id_is_safe(bad), "{bad:?} should be unsafe");
        }
        for good in ["pet", "my-plugin", "my_plugin", "a.b", "Relay123"] {
            assert!(id_is_safe(good), "{good:?} should be safe");
        }
    }

    #[test]
    fn parses_a_valid_manifest() {
        let text = r#"{"id":"pet","name":"Pet","version":"1.2.3","api_version":1,
            "entry":{"cmd":"pet.exe","args":["--x"]},"permissions":["write_pty","bogus"]}"#;
        let m = Manifest::parse(text, "pet").expect("valid");
        assert_eq!(m.id, "pet");
        assert_eq!(m.name, "Pet");
        assert_eq!(m.version, "1.2.3");
        assert_eq!(m.api_version, 1);
        assert_eq!(m.cmd, "pet.exe");
        assert_eq!(m.args, vec!["--x".to_string()]);
        // unknown permission dropped, known one kept
        assert_eq!(m.permissions, vec!["write_pty".to_string()]);
    }

    #[test]
    fn rejects_id_dir_mismatch() {
        let text = r#"{"id":"evil","entry":{"cmd":"x.exe"}}"#;
        assert!(Manifest::parse(text, "pet").is_none());
    }

    #[test]
    fn rejects_traversal_id() {
        let text = r#"{"id":"..","entry":{"cmd":"x.exe"}}"#;
        assert!(Manifest::parse(text, "..").is_none());
    }

    #[test]
    fn falls_back_to_dir_name() {
        let text = r#"{"entry":{"cmd":"x.exe"}}"#;
        let m = Manifest::parse(text, "myplugin").expect("valid");
        assert_eq!(m.id, "myplugin");
        assert_eq!(m.name, "myplugin");
    }

    #[test]
    fn requires_entry_cmd() {
        assert!(Manifest::parse(r#"{"id":"p"}"#, "p").is_none());
        assert!(Manifest::parse(r#"{"id":"p","entry":{}}"#, "p").is_none());
    }

    #[test]
    fn relative_entry_command_cannot_escape_its_plugin_directory() {
        for cmd in ["../outside", "bin/../../outside", ".", "./"] {
            let text = format!(r#"{{"id":"pet","entry":{{"cmd":"{cmd}"}}}}"#);
            assert!(Manifest::parse(&text, "pet").is_none(), "{cmd:?} should be rejected");
        }
        for cmd in ["pet.exe", "bin/pet", "./bin/pet"] {
            let text = format!(r#"{{"id":"pet","entry":{{"cmd":"{cmd}"}}}}"#);
            assert!(Manifest::parse(&text, "pet").is_some(), "{cmd:?} should be accepted");
        }
    }
}
