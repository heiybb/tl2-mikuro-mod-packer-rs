//! Corpus validation harness:
//!   verify binlayout <media_dir>    — compile every .LAYOUT with a sibling
//!                                     .BINLAYOUT and byte-compare (expect
//!                                     8965/8985 on the shipped base game)
//!   verify bindat-py <media_dir> <n> — compile N random .DATs and compare
//!                                     against Python compile_dat (needs py)
//!   verify raw <media_dir>          — rebuild the 7 RAW indices and compare
//!                                     against the shipped MEDIA/*.RAW
//!   verify mpp <media_dir> <layout...> — compile .mpp offline, print hash

use std::path::{Path, PathBuf};

use rayon::prelude::*;

fn walk(root: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(root) {
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("binlayout") => verify_binlayout(Path::new(&args[1])),
        Some("bindat") => verify_bindat_selfparse(Path::new(&args[1])),
        Some("raw") => verify_raw(Path::new(&args[1])),
        Some("mpp") => verify_mpp(&args[1..]),
        _ => {
            eprintln!("usage: verify binlayout|bindat|raw|mpp <media_dir> ...");
            std::process::exit(1);
        }
    }
}

fn verify_binlayout(media: &Path) {
    let t0 = std::time::Instant::now();
    let mut files = Vec::new();
    walk(media, &mut files);
    let pairs: Vec<(PathBuf, PathBuf)> = files
        .iter()
        .filter(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy().to_uppercase().ends_with(".BINLAYOUT"))
                .unwrap_or(false)
        })
        .filter_map(|bp| {
            let s = bp.to_string_lossy();
            let lp = PathBuf::from(&s[..s.len() - ".BINLAYOUT".len()]);
            if lp.exists() {
                Some((lp, bp.clone()))
            } else {
                None
            }
        })
        .collect();

    let results: Vec<(bool, bool, Option<String>)> = pairs
        .par_iter()
        .map(|(lp, bp)| {
            let orig = std::fs::read(bp).unwrap_or_default();
            let has_dg = orig.len() >= 6
                && u32::from_le_bytes([orig[2], orig[3], orig[4], orig[5]]) != 0;
            match mikuro_mod_rs::binlayout::compile_layout_file(lp) {
                Ok(gen) => (has_dg, gen == orig, if gen == orig { None } else {
                    Some(format!("MISMATCH {} len {} vs {}", lp.display(), gen.len(), orig.len()))
                }),
                Err(e) => (has_dg, false, Some(format!("ERR {}: {e}", lp.display()))),
            }
        })
        .collect();

    let (mut dg_pass, mut dg_total, mut nd_pass, mut nd_total, mut errs) = (0, 0, 0, 0, 0);
    let mut misses = Vec::new();
    for (has_dg, ok, msg) in results {
        if has_dg {
            dg_total += 1;
            if ok {
                dg_pass += 1;
            }
        } else {
            nd_total += 1;
            if ok {
                nd_pass += 1;
            }
        }
        if let Some(m) = msg {
            if m.starts_with("ERR") {
                errs += 1;
            }
            if misses.len() < 30 {
                misses.push(m);
            }
        }
    }
    println!("== LAYOUT->BINLAYOUT (Rust) ==");
    println!("no-datagroup: {nd_pass}/{nd_total} byte-exact");
    println!("datagroup:    {dg_pass}/{dg_total} byte-exact");
    println!("errors: {errs}   ({:.1}s)", t0.elapsed().as_secs_f64());
    for m in misses {
        println!("  {m}");
    }
}

/// Compile every .DAT and just check it parses + serializes (self-check);
/// byte comparison vs Python is done by the external cmp script.
fn verify_bindat_selfparse(media: &Path) {
    let t0 = std::time::Instant::now();
    let mut files = Vec::new();
    walk(media, &mut files);
    let dats: Vec<&PathBuf> = files
        .iter()
        .filter(|p| {
            let fu = p
                .file_name()
                .map(|f| f.to_string_lossy().to_uppercase())
                .unwrap_or_default();
            (fu.ends_with(".DAT") || fu.ends_with(".TEMPLATE") || fu.ends_with(".ANIMATION") || fu.ends_with(".HIE"))
                && !fu.contains(".BIN")
        })
        .collect();
    let (ok, err): (usize, usize) = dats
        .par_iter()
        .map(|p| match mikuro_mod_rs::bindat::compile_dat_file(p) {
            Ok(_) => (1usize, 0usize),
            Err(_) => (0, 1),
        })
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    println!(
        "bindat self-parse: {ok} ok, {err} err of {} ({:.1}s)",
        dats.len(),
        t0.elapsed().as_secs_f64()
    );
}

fn verify_raw(media: &Path) {
    for (name, gen) in [
        ("AFFIXES.RAW", mikuro_mod_rs::raw::build_affixes(media)),
        ("SKILLS.RAW", mikuro_mod_rs::raw::build_skills(media)),
        ("TRIGGERABLES.RAW", mikuro_mod_rs::raw::build_triggerables(media)),
        ("MISSILES.RAW", mikuro_mod_rs::raw::build_missiles(media)),
        ("UI.RAW", mikuro_mod_rs::raw::build_ui(media)),
        ("UNITDATA.RAW", mikuro_mod_rs::raw::build_unitdata(media)),
        ("ROOMPIECES.RAW", mikuro_mod_rs::raw::build_roompieces(media)),
    ] {
        let shipped = std::fs::read(media.join(name)).ok();
        match (gen, shipped) {
            (Some(g), Some(s)) => {
                if g == s {
                    println!("{name}: MATCH ({} bytes)", g.len());
                } else {
                    let n = g.len().min(s.len());
                    let first = (0..n).find(|&i| g[i] != s[i]).unwrap_or(n);
                    println!(
                        "{name}: DIFF gen={} shipped={} first-diff-at={first}",
                        g.len(),
                        s.len()
                    );
                }
            }
            (Some(g), None) => println!("{name}: generated {} bytes (no shipped file)", g.len()),
            (None, Some(_)) => println!("{name}: NOT generated (shipped exists!)"),
            (None, None) => println!("{name}: skipped (no source)"),
        }
    }
}

fn verify_mpp(args: &[String]) {
    let media = PathBuf::from(&args[0]);
    let ctx = mikuro_mod_rs::mpp::pipeline::Context::new(&media, None);
    for lp in &args[1..] {
        let t0 = std::time::Instant::now();
        match mikuro_mod_rs::mpp::compile_mpp(Path::new(lp), &ctx) {
            Ok(data) => {
                // compare against the sibling shipped .mpp when present
                let s = lp.to_uppercase();
                let base = &lp[..s.len() - ".LAYOUT".len()];
                let shipped = std::fs::read(format!("{base}.MPP")).ok();
                let status = match &shipped {
                    Some(sb) if *sb == data => "BYTE-EXACT vs shipped".to_string(),
                    Some(sb) => {
                        let same_hdr = sb.len() >= 24 && data.len() >= 24 && sb[..24] == data[..24];
                        let ncell = sb.len().min(data.len()).saturating_sub(24);
                        let diff = (24..24 + ncell).filter(|&i| sb[i] != data[i]).count();
                        format!(
                            "differs: hdr_match={same_hdr} len {} vs {} cell-diff {diff}/{ncell}",
                            data.len(),
                            sb.len()
                        )
                    }
                    None => "no shipped .mpp".to_string(),
                };
                println!(
                    "{lp}: {} bytes in {:.2}s — {status}",
                    data.len(),
                    t0.elapsed().as_secs_f64()
                );
            }
            Err(e) => println!("{lp}: ERROR {e}"),
        }
    }
}
