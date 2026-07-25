use std::io::Write as _;
use std::path::{Path, PathBuf};

// package the app files into a single DEFLATE-compressed payload the installer
// embeds. layout per entry: u16 name_len, name (utf8, forward slashes, path
// relative to the install root), u64 raw_len, u64 comp_len, comp bytes.
// TERMIE_PAYLOAD_DIR names a staged directory (CI); when unset the payload is
// empty and the binary is only good for --preview and ui work
fn main() {
    embed_manifest::embed_manifest(
        embed_manifest::new_manifest("termie-setup")
            .dpi_awareness(embed_manifest::manifest::DpiAwareness::PerMonitorV2),
    )
    .expect("embed manifest");
    let icon = Path::new("../assets/icon.ico");
    if icon.exists() {
        println!("cargo:rerun-if-changed=../assets/icon.ico");
        let _ = embed_resource::compile_for_everything("icon.rc", embed_resource::NONE);
    }

    println!("cargo:rerun-if-env-changed=TERMIE_PAYLOAD_DIR");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("payload.bin");
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    if let Ok(dir) = std::env::var("TERMIE_PAYLOAD_DIR") {
        let root = PathBuf::from(&dir);
        println!("cargo:rerun-if-changed={}", root.display());
        collect(&root, &root, &mut entries).unwrap_or_else(|err| panic!("invalid payload staging directory: {err}"));
        entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        assert!(
            entries.iter().any(|(n, _)| n == "termie.exe"),
            "TERMIE_PAYLOAD_DIR must contain termie.exe at its root"
        );
    }
    let mut buf = Vec::new();
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (name, raw) in &entries {
        let comp = miniz_oxide::deflate::compress_to_vec(raw, 8);
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(&(raw.len() as u64).to_le_bytes());
        buf.extend_from_slice(&(comp.len() as u64).to_le_bytes());
        buf.extend_from_slice(&comp);
    }
    let mut f = std::fs::File::create(&out).expect("create payload.bin");
    f.write_all(&buf).expect("write payload.bin");

    // the app version travels inside the payload dir as VERSION (CI writes it);
    // fall back to a dev marker so the ui always has something to show
    let ver = std::env::var("TERMIE_PAYLOAD_DIR")
        .ok()
        .and_then(|d| std::fs::read_to_string(Path::new(&d).join("VERSION")).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=TERMIE_APP_VERSION={ver}");
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|err| format!("read {}: {err}", dir.display()))?;
    for e in rd {
        let e = e.map_err(|err| format!("read entry in {}: {err}", dir.display()))?;
        let p = e.path();
        let ty = e.file_type().map_err(|err| format!("inspect {}: {err}", p.display()))?;
        if ty.is_symlink() {
            return Err(format!("payload staging must not contain symlinks: {}", p.display()));
        }
        if ty.is_dir() {
            collect(root, &p, out)?;
        } else if ty.is_file() && p.file_name().is_some_and(|n| n != "VERSION") {
            let rel = p
                .strip_prefix(root)
                .map_err(|_| format!("payload entry escaped staging directory: {}", p.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&p).map_err(|err| format!("read {}: {err}", p.display()))?;
            out.push((rel, bytes));
        } else if !ty.is_file() {
            return Err(format!("payload staging contains unsupported entry: {}", p.display()));
        }
    }
    Ok(())
}
