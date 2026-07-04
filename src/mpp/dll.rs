//! BYTE-EXACT .mpp backend: drive the REAL EditorGuts.dll through the console
//! host `TL2-Mikuro-Console.exe` (port of mpp/dll.py). Env-gated: raises a
//! descriptive error when the install / console host is missing.

use std::path::{Path, PathBuf};

use crate::packer::Overrides;

const DEFAULT_INSTALL: &str = r"E:\Torchlight 2";
const CONSOLE_EXE: &str = "TL2-Mikuro-Console.exe";

fn resolve_install(install_dir: Option<&Path>) -> PathBuf {
    if let Some(d) = install_dir {
        return d.to_path_buf();
    }
    if let Ok(d) = std::env::var("TL2_INSTALL_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Ok(m) = std::env::var("TL2_MEDIA_DIR") {
        if !m.is_empty() {
            let p = PathBuf::from(m.trim_end_matches(['\\', '/']));
            if let Some(parent) = p.parent() {
                return parent.to_path_buf();
            }
        }
    }
    PathBuf::from(DEFAULT_INSTALL)
}

fn read_mod_name(mod_dir: &Path) -> String {
    let fallback = mod_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mod_dat = mod_dir.join("MOD.DAT");
    if !mod_dat.is_file() {
        return fallback;
    }
    let data = match std::fs::read(&mod_dat) {
        Ok(d) => d,
        Err(_) => return fallback,
    };
    let text = crate::text::decode_utf16le_lossy(&data);
    for key in ["MOD_FILE_NAME", "NAME"] {
        let re = regex::Regex::new(&format!(r"<STRING>{key}:([^\r\n]+)")).unwrap();
        if let Some(c) = re.captures(&text) {
            return c[1].trim().to_string();
        }
    }
    fallback
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let p = e.path();
        let d = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&p, &d)?;
        } else {
            std::fs::copy(&p, &d)?;
        }
    }
    Ok(())
}

fn count_and_collect_mpp(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(p);
                } else if p
                    .file_name()
                    .map(|f| f.to_string_lossy().to_uppercase().ends_with(".MPP"))
                    .unwrap_or(false)
                {
                    out.push(p);
                }
            }
        }
    }
    out
}

pub struct DllRunResult {
    pub mod_name: String,
    pub returncode: Option<i32>,
    pub mpp_count: usize,
    pub mpp_copied_back: usize,
    pub staged: bool,
}

/// Drive the real EditorGuts.dll to produce byte-exact .mpp files for the mod
/// at `mod_dir` (MOD.DAT + MEDIA/). Success is measured by .mpp COUNT — the
/// console host routinely exits with a harmless teardown crash code AFTER
/// writing every file.
pub fn regen_mpp_via_dll(mod_dir: &Path, install_dir: Option<&Path>) -> Result<DllRunResult, String> {
    let install = resolve_install(install_dir);
    let exe = install.join(CONSOLE_EXE);
    if !install.is_dir() {
        return Err(format!(
            "TL2 install dir not found: {} (set TL2_INSTALL_DIR)",
            install.display()
        ));
    }
    if !exe.is_file() {
        return Err(format!(
            "console host not found: {} (the byte-exact DLL backend needs {CONSOLE_EXE} in the install dir)",
            exe.display()
        ));
    }
    if !mod_dir.is_dir() {
        return Err(format!("mod dir not found: {}", mod_dir.display()));
    }

    let mods_root = install.join("mods");
    let name = read_mod_name(mod_dir);
    let expected = mods_root.join(&name);
    let staged = !mod_dir
        .to_string_lossy()
        .eq_ignore_ascii_case(&expected.to_string_lossy());
    let build_dir = if staged {
        std::fs::create_dir_all(&mods_root).map_err(|e| e.to_string())?;
        if expected.exists() {
            std::fs::remove_dir_all(&expected).map_err(|e| e.to_string())?;
        }
        copy_tree(mod_dir, &expected).map_err(|e| e.to_string())?;
        expected.clone()
    } else {
        mod_dir.to_path_buf()
    };

    let run = (|| -> Result<DllRunResult, String> {
        let proc = std::process::Command::new(&exe)
            .args(["build", &name, "--clean", "--twice"])
            .current_dir(&install)
            .env("MSYS_NO_PATHCONV", "1")
            .output()
            .map_err(|e| format!("failed to launch console host: {e}"))?;

        let mpps = count_and_collect_mpp(&build_dir);
        if mpps.is_empty() {
            return Err(format!(
                "DLL MPP build produced no .mpp (console exit {:?})",
                proc.status.code()
            ));
        }
        let mut copied = 0usize;
        if staged {
            for p in &mpps {
                if let Ok(rel) = p.strip_prefix(&build_dir) {
                    let dst = mod_dir.join(rel);
                    if let Some(parent) = dst.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::copy(p, &dst).is_ok() {
                        copied += 1;
                    }
                }
            }
        }
        Ok(DllRunResult {
            mod_name: name.clone(),
            returncode: proc.status.code(),
            mpp_count: mpps.len(),
            mpp_copied_back: copied,
            staged,
        })
    })();

    if staged && expected.is_dir() {
        let _ = std::fs::remove_dir_all(&expected);
    }
    run
}

/// packer.generate_mpp_files backend='dll': in-place (editor writes .mpp next
/// to the layouts) or overrides mode (regen on a throwaway copy, harvest the
/// byte-exact .mpp into `overrides`).
pub fn generate_mpp_files_dll(
    media_dir: &Path,
    overrides: Option<&mut Overrides>,
    install_dir: Option<&Path>,
) -> Result<usize, String> {
    let mod_dir = media_dir
        .parent()
        .ok_or_else(|| "media dir has no parent".to_string())?
        .to_path_buf();

    match overrides {
        None => {
            let res = regen_mpp_via_dll(&mod_dir, install_dir)?;
            Ok(if res.mpp_copied_back > 0 {
                res.mpp_copied_back
            } else {
                res.mpp_count
            })
        }
        Some(ov) => {
            let tmp = std::env::temp_dir().join(format!(
                "mikuro_mpp_dll_{}",
                mod_dir
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default()
            ));
            if tmp.exists() {
                std::fs::remove_dir_all(&tmp).map_err(|e| e.to_string())?;
            }
            copy_tree(&mod_dir, &tmp).map_err(|e| e.to_string())?;
            let run = regen_mpp_via_dll(&tmp, install_dir);
            let result = run.and_then(|_res| {
                let tmp_media = tmp.join("MEDIA");
                let mut n = 0usize;
                for p in count_and_collect_mpp(&tmp_media) {
                    if let Ok(rel) = p.strip_prefix(&tmp_media) {
                        let key = rel.to_string_lossy().replace('\\', "/").to_uppercase();
                        if let Ok(bytes) = std::fs::read(&p) {
                            ov.insert(key, bytes);
                            n += 1;
                        }
                    }
                }
                Ok(n)
            });
            let _ = std::fs::remove_dir_all(&tmp);
            result
        }
    }
}
