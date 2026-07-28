//! End-to-end: unpack real third-party `.MOD` files and record every `UNK_` placeholder.
//!
//! Run twice, once per dictionary, then diff the two reports:
//!   set TL2_UNK_DIR=<mods dir>  TL2_UNK_FILTER=<substring>  TL2_UNK_OUT=<report path>
//!   cargo test --release --test unk_ab -- --ignored --nocapture
//!   TL2_LEGACY_DAT_KEYS=1 ... (same, different TL2_UNK_OUT)
//!
//! The two runs MUST be separate processes: `dat_keys::KEYS` is a `LazyLock` built once.

use std::path::PathBuf;
use tl2_mikuro_mod_packer_rs::unpack::{unpack_mod, Options};

fn targets() -> Vec<PathBuf> {
    let dir = std::env::var("TL2_UNK_DIR").unwrap_or_else(|_| r"E:/Torchlight 2/mods".into());
    let filters: Vec<String> = std::env::var("TL2_UNK_FILTER")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::from(dir)];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x.eq_ignore_ascii_case("mod")).unwrap_or(false) {
                let n = p.file_name().unwrap().to_string_lossy().to_string();
                if filters.is_empty() || filters.iter().any(|f| n.contains(f.as_str())) {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

#[test]
#[ignore = "unpacks the machine's installed mods"]
fn count_unknown_names() {
    let tmp = std::env::temp_dir().join("tl2_unk_ab");
    let opt = Options { raw: false, restore_times: false, write_mod_dat: false, filter: None };
    let (mut files, mut dats) = (0usize, 0usize);
    let mut rows: Vec<String> = Vec::new();
    let list = targets();
    for m in &list {
        let out = tmp.join(m.file_stem().unwrap());
        let _ = std::fs::remove_dir_all(&out);
        match unpack_mod(m, &out, &opt) {
            Ok(r) => {
                files += r.files;
                dats += r.dats;
                for n in &r.unknown_names {
                    rows.push(format!("{}\t{}", m.file_name().unwrap().to_string_lossy(), n));
                }
                println!(
                    "  {:<52} files={:<6} dats={:<6} UNK={}",
                    m.file_name().unwrap().to_string_lossy(),
                    r.files,
                    r.dats,
                    r.unknown_names.len()
                );
            }
            Err(e) => println!("  [skip] {}: {e}", m.display()),
        }
        let _ = std::fs::remove_dir_all(&out);
    }
    rows.sort();
    let legacy = std::env::var_os("TL2_LEGACY_DAT_KEYS").is_some();
    println!(
        "\nMODE={}  mods={}  files={files}  dats={dats}  UNK_行={}",
        if legacy { "legacy-2888" } else { "tiered-465k" },
        list.len(),
        rows.len()
    );
    if let Ok(p) = std::env::var("TL2_UNK_OUT") {
        std::fs::write(&p, rows.join("\n")).expect("write report");
        println!("-> {p}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
