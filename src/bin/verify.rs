//! Corpus validation harness:
//!   verify binlayout <media_dir>    — compile every .LAYOUT with a sibling
//!                                     .BINLAYOUT and byte-compare (expect
//!                                     8966/8985 on the shipped base game; the
//!                                     19 misses are the editor's uninitialised
//!                                     @25 bytes, unrecoverable from source.
//!                                     A .BINLAYOUT with no .LAYOUT beside it
//!                                     is not a pair and is not counted)
//!   verify bindat-py <media_dir> <n> — compile N random .DATs and compare
//!                                     against Python compile_dat (needs py)
//!   verify raw <media_dir>          — rebuild the 7 RAW indices and compare
//!                                     against the shipped MEDIA/*.RAW
//!   verify mpp <media_dir> <layout...> — compile .mpp offline, print hash
//!   verify binlayout-roundtrip <media_dir> — decompile every .BINLAYOUT and
//!                                     re-compile it; must be byte-identical
//!   verify unpack <mod.MOD>         — unpack a real .MOD, repack the tree, and
//!                                     compare the two containers entry by entry
//!   verify mpp-ab <media_dir> [rel_subdir]
//!                                   — for every .LAYOUT with a GUTS-baked sibling
//!                                     .MPP, bake offline and report the per-cell
//!                                     CONFUSION MATRIX (walk/wall/void) plus the
//!                                     same split by distance-to-grid-border, so a
//!                                     systematic edge/threshold bias is visible
//!                                     rather than hidden in one diff percentage.
//!   verify mpp-info <mod_media> <base_media> <layout>... [--pieces]
//!                                   — region/header provenance for one tile, vs
//!                                     the shipped sibling header; --pieces adds
//!                                     which piece set each hull extremum (and
//!                                     which pieces were silently dropped)
//!   verify mpp-boxfit <media_dir>   — CSV of collision hull vs the box GUTS
//!                                     shipped, for fitting the snap rule
//!   verify mpp-audit <mod_media> <base_media> [--verbose]
//!                                   — the pack-time room-block interface check,
//!                                     run over a tree without packing

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
        Some("binlayout-roundtrip") => verify_binlayout_roundtrip(Path::new(&args[1])),
        Some("unpack") => verify_unpack(Path::new(&args[1])),
        Some("bindat") => verify_bindat_selfparse(Path::new(&args[1])),
        Some("bindat-corpus") => verify_bindat_corpus(Path::new(&args[1]), Path::new(&args[2])),
        Some("raw") => verify_raw(Path::new(&args[1])),
        Some("mpp") => verify_mpp(&args[1..]),
        Some("mpp-corpus") => verify_mpp_corpus(Path::new(&args[1]), Path::new(&args[2])),
        Some("mpp-ab") => verify_mpp_ab(&args[1..]),
        Some("mpp-info") => mpp_info(&args[1..]),
        Some("mpp-audit") => mpp_audit(&args[1..]),
        Some("mpp-boxfit") => mpp_boxfit(&args[1..]),
        Some("mpp-tris") => mpp_tris(&args[1..]),
        Some("ce-audit") => ce_audit(&args[1..]),
        // Where would a pack look for base data on THIS machine? A wrong answer
        // here degrades silently (see `packer::tl2_media_dir`), so it is worth
        // being able to ask directly instead of inferring it from a bake.
        Some("base-probe") => {
            use tl2_mikuro_mod_packer_rs::packer::{discover_tl2_installs, tl2_media_dir};
            match tl2_media_dir() {
                Some(media) => {
                    println!("TL2_MEDIA_DIR = {}", media.display());
                    println!("  exists: {}   LEVELSETS: {}",
                             media.is_dir(), media.join("LEVELSETS").is_dir());
                }
                None => println!("TL2_MEDIA_DIR is not set (nothing is guessed)"),
            }
            // Shown on request only — this is what the TUI would offer if the
            // user authorises a search, never what it applies on its own.
            let found = discover_tl2_installs();
            println!("
installs a search WOULD offer ({}):", found.len());
            for p in &found {
                println!("  {}   MEDIA:{}  PAKS/DATA.PAK:{}",
                         p.display(), p.join("MEDIA").is_dir(),
                         p.join("PAKS/DATA.PAK").is_file());
            }
        }
        Some("mpp-gen") => mpp_gen(&args[1..]),
        _ => {
            eprintln!("usage: verify binlayout|binlayout-roundtrip|bindat|bindat-corpus|raw|mpp|mpp-gen <media_dir> ...");
            eprintln!("       verify unpack <mod.MOD>   — unpack, repack, compare entry contents");
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
            match tl2_mikuro_mod_packer_rs::binlayout::compile_layout_file(lp) {
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

/// UNPACK direction: every .BINLAYOUT in the tree must decompile to text that
/// re-compiles to the SAME bytes. Two passes, because they answer different
/// questions:
///   * shipped  — input is the game's own .BINLAYOUT (also exercises bytes our
///                compiler would not itself produce);
///   * selfgen  — input is OUR compile of the .LAYOUT source (the fixed point
///                that unpack→repack of a mod actually relies on).
fn verify_binlayout_roundtrip(media: &Path) {
    use tl2_mikuro_mod_packer_rs::binlayout::compile_layout_bytes;
    use tl2_mikuro_mod_packer_rs::binlayout_decompile::{decompile_binlayout, to_layout_bytes};

    let t0 = std::time::Instant::now();
    let mut files = Vec::new();
    walk(media, &mut files);
    let bins: Vec<PathBuf> = files
        .iter()
        .filter(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy().to_uppercase().ends_with(".BINLAYOUT"))
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    // (shipped_ok, shipped_total, selfgen_ok, selfgen_total, message)
    let results: Vec<(bool, bool, bool, bool, Option<String>)> = bins
        .par_iter()
        .map(|bp| {
            let shipped = std::fs::read(bp).unwrap_or_default();
            let roundtrip = |orig: &[u8]| -> Result<Vec<u8>, String> {
                let text = decompile_binlayout(orig)?;
                compile_layout_bytes(&to_layout_bytes(&text))
            };
            let mut msg = None;
            let ship_ok = match roundtrip(&shipped) {
                Ok(re) => {
                    if re != shipped {
                        msg = Some(format!(
                            "SHIPPED MISMATCH {} at byte {} ({} vs {} bytes)",
                            bp.display(),
                            first_diff(&re, &shipped),
                            re.len(),
                            shipped.len()
                        ));
                    }
                    re == shipped
                }
                Err(e) => {
                    msg = Some(format!("SHIPPED ERR {}: {e}", bp.display()));
                    false
                }
            };

            // The .LAYOUT source next to it, compiled by us, is the fixed point.
            let s = bp.to_string_lossy();
            let src = PathBuf::from(&s[..s.len() - ".BINLAYOUT".len()]);
            let (self_ok, self_has) = match compile_layout_bytes(&std::fs::read(&src).unwrap_or_default()) {
                Ok(ours) => match roundtrip(&ours) {
                    Ok(re) => {
                        if re != ours && msg.is_none() {
                            msg = Some(format!(
                                "SELFGEN MISMATCH {} at byte {}",
                                bp.display(),
                                first_diff(&re, &ours)
                            ));
                        }
                        (re == ours, true)
                    }
                    Err(e) => {
                        if msg.is_none() {
                            msg = Some(format!("SELFGEN ERR {}: {e}", bp.display()));
                        }
                        (false, true)
                    }
                },
                // No parseable source next to it — not a round-trip failure.
                Err(_) => (false, false),
            };
            (ship_ok, true, self_ok, self_has, msg)
        })
        .collect();

    let (mut so, mut st, mut fo, mut ft) = (0, 0, 0, 0);
    let mut misses = Vec::new();
    for (ship_ok, ship_has, self_ok, self_has, msg) in results {
        st += ship_has as usize;
        so += (ship_has && ship_ok) as usize;
        ft += self_has as usize;
        fo += (self_has && self_ok) as usize;
        if let Some(m) = msg {
            if misses.len() < 30 {
                misses.push(m);
            }
        }
    }
    println!("== BINLAYOUT->LAYOUT round-trip (decompile then recompile) ==");
    println!("shipped input: {so}/{st} byte-exact");
    println!("our compile:   {fo}/{ft} byte-exact (fixed point)");
    println!("({:.1}s)", t0.elapsed().as_secs_f64());
    for m in misses {
        println!("  {m}");
    }
}

/// UNPACK end-to-end: unpack a real `.MOD`, repack the tree, and compare the
/// two containers ENTRY BY ENTRY on decompressed content. Content equality is
/// the criterion rather than raw bytes, because a repack legitimately differs in
/// the header's gameVersion, in the BINDAT string-id scheme when the input came
/// from GUTS, and in where the generated `.RAW` records sort (see unpack docs).
fn verify_unpack(mod_path: &Path) {
    use tl2_mikuro_mod_packer_rs::packer::{convert_all, pack_mod, Overrides};
    use tl2_mikuro_mod_packer_rs::pak::PakReader;
    use tl2_mikuro_mod_packer_rs::unpack::{unpack_mod_with_progress, Options};

    let t0 = std::time::Instant::now();
    let tmp = std::env::temp_dir().join(format!("tl2verify_unpack_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let tree = tmp.join("tree");

    // Pin the gameVersion to the input's, so the header comparison is about the
    // unpack rather than about which game build this machine has installed.
    if let Ok(r) = PakReader::open_mod(mod_path) {
        if let Some(h) = r.header() {
            std::env::set_var("TL2_MOD_GAMEVER", format!("0x{:016X}", h.gamever));
        }
    }
    let last = std::sync::Mutex::new(std::time::Instant::now());
    let rep = match unpack_mod_with_progress(mod_path, &tree, &Options::default(), |d, t| {
        let Ok(mut last) = last.lock() else { return };
        if d == t || last.elapsed().as_millis() >= 500 {
            *last = std::time::Instant::now();
            print!("\r  unpacking {d}/{t}   ");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    }) {
        Ok(r) => r,
        Err(e) => {
            println!("\nunpack failed: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "\n  unpacked {} files ({} DAT, {} LAYOUT, {} verbatim)",
        rep.files, rep.dats, rep.layouts, rep.copied
    );

    println!("  repacking…");
    let media = tree.join("MEDIA");
    let mut ov = Overrides::new();
    convert_all(&media, Some(&mut ov), "none", "auto");
    let out = tmp.join("repacked.MOD");
    if let Err(e) = pack_mod(&media, &out, Some(&tree), Some(&ov)) {
        println!("repack failed: {e}");
        std::process::exit(1);
    }

    // Compare content per entry.
    let mut a = PakReader::open_mod(mod_path).expect("reopen input");
    let mut b = PakReader::open_mod(&out).expect("open repacked");
    let ea: Vec<_> = a.entries().to_vec();
    let eb: std::collections::HashMap<String, _> = b
        .entries()
        .iter()
        .map(|e| (e.path.to_uppercase(), e.clone()))
        .collect();
    let (mut same, mut diff, mut missing) = (0usize, 0usize, 0usize);
    let mut msgs = Vec::new();
    for e in &ea {
        let Some(f) = eb.get(&e.path.to_uppercase()) else {
            missing += 1;
            if msgs.len() < 20 {
                msgs.push(format!("MISSING {}", e.path));
            }
            continue;
        };
        let ca = a.read_entry(e).unwrap_or_default();
        let cb = b.read_entry(f).unwrap_or_default();
        if ca == cb {
            same += 1;
        } else {
            diff += 1;
            if msgs.len() < 20 {
                msgs.push(format!(
                    "CONTENT {} ({} vs {} bytes, first diff @{})",
                    e.path,
                    ca.len(),
                    cb.len(),
                    first_diff(&ca, &cb)
                ));
            }
        }
    }
    std::env::remove_var("TL2_MOD_GAMEVER");
    println!("== UNPACK → REPACK ({}) ==", mod_path.display());
    println!("identical content: {same}/{}", ea.len());
    println!("differing: {diff}   missing: {missing}   ({:.1}s)", t0.elapsed().as_secs_f64());
    let raw_a = std::fs::read(mod_path).unwrap_or_default();
    let raw_b = std::fs::read(&out).unwrap_or_default();
    println!("raw bytes identical: {}", raw_a == raw_b);
    for m in msgs {
        println!("  {m}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Offset of the first differing byte (or the shorter length).
fn first_diff(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()))
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
        .map(|p| match tl2_mikuro_mod_packer_rs::bindat::compile_dat_file(p) {
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

/// Compile every *.DAT with a shipped sibling *.DAT.BINDAT using the GLOBAL
/// corpus dict and byte-compare (the Python `bindat.py verify` equivalent —
/// expect ~15976/16084 on the base game). Also times hash-id mode on the same
/// set for a like-for-like performance comparison.
fn verify_bindat_corpus(media: &Path, dict_path: &Path) {
    let t0 = std::time::Instant::now();
    let dict = tl2_mikuro_mod_packer_rs::bindat::CorpusDict::load(dict_path).expect("load corpus dict");
    let load_s = t0.elapsed().as_secs_f64();

    let mut files = Vec::new();
    walk(media, &mut files);
    let pairs: Vec<(PathBuf, PathBuf)> = files
        .iter()
        .filter(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy().to_uppercase().ends_with(".DAT.BINDAT"))
                .unwrap_or(false)
        })
        .filter_map(|bp| {
            let s = bp.to_string_lossy();
            let dp = PathBuf::from(&s[..s.len() - ".BINDAT".len()]);
            if dp.exists() {
                Some((dp, bp.clone()))
            } else {
                None
            }
        })
        .collect();

    // timed: compile only (compare against shipped happens untimed afterwards)
    let t1 = std::time::Instant::now();
    let outputs: Vec<Result<Vec<u8>, String>> = pairs
        .par_iter()
        .map(|(dp, _bp)| tl2_mikuro_mod_packer_rs::bindat::compile_dat_file_corpus(dp, &dict))
        .collect();
    let corpus_s = t1.elapsed().as_secs_f64();

    let t2 = std::time::Instant::now();
    let hash_ok: usize = pairs
        .par_iter()
        .map(|(dp, _bp)| tl2_mikuro_mod_packer_rs::bindat::compile_dat_file(dp).is_ok() as usize)
        .sum();
    let hash_s = t2.elapsed().as_secs_f64();

    let results: Vec<(bool, bool)> = pairs
        .par_iter()
        .zip(&outputs)
        .map(|((_dp, bp), out)| match out {
            Ok(gen) => (*gen == std::fs::read(bp).unwrap_or_default(), false),
            Err(_) => (false, true),
        })
        .collect();
    let ok = results.iter().filter(|(m, _e)| *m).count();
    let err = results.iter().filter(|(_m, e)| *e).count();
    println!("== DAT->BINDAT corpus-dict mode (GLOBAL ids, == GUTS sub_10289A40) ==");
    println!(
        "dict: {} strings, max_id={:#x}, loaded in {load_s:.2}s",
        dict.len(),
        dict.max_id
    );
    println!(
        "{} files: {ok} byte-exact vs shipped, {} diff, {err} err   ({corpus_s:.2}s)",
        pairs.len(),
        pairs.len() - ok - err
    );
    println!(
        "hash-id mode on same {} files: {hash_ok} compiled   ({hash_s:.2}s)  -> corpus overhead {:+.1}%",
        pairs.len(),
        (corpus_s / hash_s - 1.0) * 100.0
    );
}

fn verify_raw(media: &Path) {
    for (name, gen) in [
        ("AFFIXES.RAW", tl2_mikuro_mod_packer_rs::raw::build_affixes(media)),
        ("SKILLS.RAW", tl2_mikuro_mod_packer_rs::raw::build_skills(media)),
        ("TRIGGERABLES.RAW", tl2_mikuro_mod_packer_rs::raw::build_triggerables(media)),
        ("MISSILES.RAW", tl2_mikuro_mod_packer_rs::raw::build_missiles(media)),
        ("UI.RAW", tl2_mikuro_mod_packer_rs::raw::build_ui(media)),
        ("UNITDATA.RAW", tl2_mikuro_mod_packer_rs::raw::build_unitdata(media)),
        ("ROOMPIECES.RAW", tl2_mikuro_mod_packer_rs::raw::build_roompieces(media)),
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

/// Full-corpus MPP A/B vs the DLL-oracle regen corpus (tools/mpp_corpus):
/// for every oracle .MPP, compile the matching MEDIA/LAYOUTS .LAYOUT offline
/// and compare header dims + per-cell bytes. Expected to land on the
/// documented ~99.71% cell floor if the Rust port equals the Python one.
fn verify_mpp_corpus(media: &Path, corpus: &Path) {
    let t0 = std::time::Instant::now();
    let ctx = tl2_mikuro_mod_packer_rs::mpp::pipeline::Context::new(media, None);
    let ctx_s = t0.elapsed().as_secs_f64();

    let mut oracle_files = Vec::new();
    walk(corpus, &mut oracle_files);
    let oracle: Vec<PathBuf> = oracle_files
        .into_iter()
        .filter(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy().to_uppercase().ends_with(".MPP"))
                .unwrap_or(false)
        })
        .collect();

    struct Row {
        byte_exact: bool,
        dim_match: bool,
        cells: u64,
        diff: u64,
        err: bool,
    }
    let t1 = std::time::Instant::now();
    let rows: Vec<Row> = oracle
        .par_iter()
        .map(|op| {
            let rel = op.strip_prefix(corpus).unwrap_or(op);
            let rels = rel.to_string_lossy();
            let lp = media
                .join("LAYOUTS")
                .join(format!("{}.LAYOUT", &rels[..rels.len() - ".MPP".len()]));
            let shipped = std::fs::read(op).unwrap_or_default();
            let gen = match tl2_mikuro_mod_packer_rs::mpp::compile_mpp(&lp, &ctx) {
                Ok(g) => g,
                Err(_) => {
                    return Row { byte_exact: false, dim_match: false, cells: 0, diff: 0, err: true }
                }
            };
            let byte_exact = gen == shipped;
            let dim_match =
                gen.len() >= 8 && shipped.len() >= 8 && gen[..8] == shipped[..8] && gen.len() == shipped.len();
            let (cells, diff) = if dim_match {
                let n = (gen.len() - 24) as u64;
                let d = gen[24..]
                    .iter()
                    .zip(&shipped[24..])
                    .filter(|(a, b)| a != b)
                    .count() as u64;
                (n, d)
            } else {
                (0, 0)
            };
            Row { byte_exact, dim_match, cells, diff, err: false }
        })
        .collect();
    let compile_s = t1.elapsed().as_secs_f64();

    let n = rows.len();
    let byte_exact = rows.iter().filter(|r| r.byte_exact).count();
    let dim_match = rows.iter().filter(|r| r.dim_match).count();
    let errs = rows.iter().filter(|r| r.err).count();
    let cells: u64 = rows.iter().map(|r| r.cells).sum();
    let diff: u64 = rows.iter().map(|r| r.diff).sum();
    println!("== MPP full-corpus A/B vs DLL oracle (Rust offline backend) ==");
    println!("context build: {ctx_s:.1}s   compile+compare {n} tiles: {compile_s:.1}s");
    println!("byte-exact files: {byte_exact}/{n}   dim-match: {dim_match}/{n}   errors: {errs}");
    println!(
        "cells (dim-matched): {cells}   diff: {diff}   accuracy: {:.4}%",
        100.0 * (cells - diff) as f64 / cells.max(1) as f64
    );
    if std::env::var("MPP_TIMING").map(|v| v == "1").unwrap_or(false) {
        println!("{}", tl2_mikuro_mod_packer_rs::mpp::native::timing_report());
    }
}

/// For each of the four hull extrema, the pieces that reach furthest — using the
/// region walk's own attribution, so what is printed is exactly what set the box.
/// A piece reached through a Layout Link is labelled with the link chain, which is
/// the distinction that matters: whether the over-reach is a direct room piece or
/// something a link dragged in.
fn print_hull_extremes(
    layout: &tl2_mikuro_mod_packer_rs::mpp::layout::Layout,
    ctx: &tl2_mikuro_mod_packer_rs::mpp::pipeline::Context,
) {
    use tl2_mikuro_mod_packer_rs::mpp::region::{hull_contributors, HullContrib};

    // Why each Room Piece did or did not reach the hull. Several of the skip
    // paths are silent in the bake (an unknown GUID just `continue`s and is NOT
    // counted by n_pieces_missing), and a silently dropped piece is exactly what
    // an under-sized box looks like.
    {
        use tl2_mikuro_mod_packer_rs::mpp::region::{collision_enabled, select_collision_file};
        let mut tally: std::collections::BTreeMap<&str, usize> = Default::default();
        let mut dropped_west: Vec<(f64, String, &str)> = Vec::new();
        for pi in layout.room_piece_indices() {
            let o = &layout.objects[pi];
            let reason = if !collision_enabled(o) {
                "COLLISION ENABLED:false"
            } else if !ctx.guid.contains_key(&o.guid) {
                "GUID not in LEVELSETS (silent skip)"
            } else {
                let pd = &ctx.guid[&o.guid];
                if select_collision_file(pd, o.visual).is_empty() {
                    "no COLLISIONFILE"
                } else if ctx.get_bounds(select_collision_file(pd, o.visual)).is_none() {
                    "collision mesh unloadable"
                } else {
                    "reached the hull (or a later gate)"
                }
            };
            *tally.entry(reason).or_default() += 1;
            if reason != "reached the hull (or a later gate)" {
                // Report the piece's would-be COLLISION extent, not its origin:
                // an under-sized box is a question about extents.
                let mut lbl = String::new();
                let mut west = o.world.m[0][3];
                if let Some(pd) = ctx.guid.get(&o.guid) {
                    let cf = select_collision_file(pd, o.visual);
                    if !cf.is_empty() {
                        if let Some(b) = ctx.get_bounds(cf) {
                            use tl2_mikuro_mod_packer_rs::mpp::region::transform_bounds_f32;
                            let (mnx, _, mxx, _) = transform_bounds_f32(&b, &o.world);
                            west = mnx as f64;
                            lbl = format!(
                                "{} [{}] collx[{:.3},{:.3}]{}{}",
                                pd.name, cf.rsplit('/').next().unwrap_or(cf), mnx, mxx,
                                if pd.alwaysbakecollision { " ALWAYSBAKECOLLISION" } else { "" },
                                if pd.neverbake { " NEVERBAKE" } else { "" },
                            );
                        }
                    }
                }
                if lbl.is_empty() {
                    lbl = format!("{} guid={}", o.name, o.guid);
                }
                dropped_west.push((west, lbl, reason));
            }
        }
        println!("  Room Piece disposition:");
        for (k, v) in &tally {
            println!("      {v:>5}  {k}");
        }
        dropped_west.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        for (x, n, r) in dropped_west.iter().take(8) {
            println!("      dropped, reaches x={x:>9.3}  {n}
          ({r})");
        }
    }

    let cs = hull_contributors(layout, ctx);
    let n_link = cs.iter().filter(|c| !c.via.is_empty()).count();
    println!(
        "  hull contributors: {} ({} direct room pieces, {n_link} via Layout Links)",
        cs.len(),
        cs.len() - n_link
    );
    let show = |c: &HullContrib, v: f32| {
        let sc = if (c.scale.0 - 1.0).abs() > 1e-6
            || (c.scale.1 - 1.0).abs() > 1e-6
            || (c.scale.2 - 1.0).abs() > 1e-6
        {
            format!("  scale=({:.3},{:.3},{:.3})", c.scale.0, c.scale.1, c.scale.2)
        } else {
            String::new()
        };
        println!(
            "      {v:>12.4}  {} [{}]{}{}{sc}",
            c.piece,
            c.mesh,
            if c.flags.is_empty() { String::new() } else { format!("  {}", c.flags) },
            if c.via.is_empty() { String::new() } else { format!("
                    via {}", c.via) }
        );
    };
    for (axis, get, want_min) in [
        ("min x", (|c: &HullContrib| c.minx) as fn(&HullContrib) -> f32, true),
        ("max x", |c: &HullContrib| c.maxx, false),
        ("min z", |c: &HullContrib| c.minz, true),
        ("max z", |c: &HullContrib| c.maxz, false),
    ] {
        let mut v: Vec<&HullContrib> = cs.iter().collect();
        v.sort_by(|a, b| {
            let (x, y) = (get(a), get(b));
            if want_min { x.partial_cmp(&y).unwrap() } else { y.partial_cmp(&x).unwrap() }
        });
        println!("    {axis}:");
        for (rank, c) in v.iter().take(3).enumerate() {
            show(c, get(c));
            // Only the top contributor actually sets the box, so print its ancestor
            // chain: that is where the editor's base/detail split reads from.
            if rank == 0 {
                for (d, a) in c.chain.iter().enumerate() {
                    println!("                    anc[{d}] {a}");
                }
            }
        }
    }
}

/// ce-audit <mod_media> <base_media> <layout>...
/// Group a tile's `COLLISION ENABLED:false` Room Pieces by piece definition, so the
/// ground pieces can be told from the props.
///
/// The flag removes a piece from BOTH the region box and the triangle gather (gate2
/// @0x10203b62) while leaving its visual mesh, so on a PROP it usefully clears an
/// obstacle, and on a GROUND piece it deletes the floor from the walk grid and leaves
/// terrain the player can see but not stand on.
fn ce_audit(args: &[String]) {
    use tl2_mikuro_mod_packer_rs::mpp::layout::load_layout_file;
    use tl2_mikuro_mod_packer_rs::mpp::pipeline::Context;
    use tl2_mikuro_mod_packer_rs::mpp::region::{collision_enabled, select_collision_file};

    let ctx = Context::new(Path::new(&args[0]), Some(Path::new(&args[1])));
    // Substrings that mark a piece as walkable ground rather than decoration.
    const GROUND: [&str; 12] = [
        "FLOOR", "GROUND", "SLOPE", "DIRT", "SAND", "PATH", "TERRAIN", "BED", "PLAZA",
        "STAIR", "RAMP", "BRIDGE",
    ];
    for lp in args[2..].iter().filter(|a| !a.starts_with("--")) {
        let p = Path::new(lp);
        let Ok(layout) = load_layout_file(p) else {
            println!("{lp}: parse error");
            continue;
        };
        // bucket: 0 = CE:false, 1 = has no COLLISIONFILE at all, 2 = contributes
        let mut by: std::collections::HashMap<(u8, String), (usize, String, String)> =
            Default::default();
        let mut total = 0usize;
        for pi in layout.room_piece_indices() {
            let o = &layout.objects[pi];
            total += 1;
            let has_coll = ctx
                .guid
                .get(&o.guid)
                .map(|pd| !select_collision_file(pd, o.visual).is_empty())
                .unwrap_or(false);
            let bucket = if !collision_enabled(o) {
                0u8
            } else if !has_coll {
                1
            } else {
                2
            };
            if bucket == 2 {
                continue;
            }
            let (name, mesh) = match ctx.guid.get(&o.guid) {
                Some(pd) => (
                    if pd.name.is_empty() { "<unnamed>".to_string() } else { pd.name.clone() },
                    select_collision_file(pd, o.visual).to_string(),
                ),
                None => ("<GUID not in LEVELSETS>".to_string(), String::new()),
            };
            let e = by.entry((bucket, o.guid.clone())).or_insert((0, name, mesh));
            e.0 += 1;
        }
        let mut rows: Vec<_> = by.into_iter().collect();
        rows.sort_by_key(|((b, _), v)| (*b, std::cmp::Reverse(v.0)));
        let ce: usize = rows.iter().filter(|((b, _), _)| *b == 0).map(|(_, v)| v.0).sum();
        let nocoll: usize = rows.iter().filter(|((b, _), _)| *b == 1).map(|(_, v)| v.0).sum();
        println!(
            "
===== {} =====
  Room Pieces {total}   CE:false {ce} ({:.0}%)   无碰撞网格 {nocoll}",
            p.file_name().unwrap_or_default().to_string_lossy(),
            100.0 * ce as f64 / total.max(1) as f64
        );
        println!("  {:>5}  {:<34} {:<46} 判定", "数量", "件名", "碰撞网格");
        let mut last = 9u8;
        for ((b, guid), (n, name, mesh)) in &rows {
            if *b != last {
                println!(
                    "  -- {} --",
                    if *b == 0 { "COLLISION ENABLED:false" } else { "件定义里没有 COLLISIONFILE" }
                );
                last = *b;
            }
            let up = format!("{} {}", name.to_uppercase(), mesh.to_uppercase());
            let is_ground = GROUND.iter().any(|g| up.contains(g));
            println!(
                "  {:>5}  {:<34} {:<44} {}",
                n,
                name,
                if mesh.is_empty() { "(无)" } else { mesh.rsplit('/').next().unwrap_or(mesh) },
                if is_ground { "<<< 地面类" } else { "道具" }
            );
            let _ = guid;
        }
        let ground: usize = rows
            .iter()
            .filter(|(_, (_, name, mesh))| {
                let up = format!("{} {}", name.to_uppercase(), mesh.to_uppercase());
                GROUND.iter().any(|g| up.contains(g))
            })
            .map(|(_, v)| v.0)
            .sum();
        println!("  -> 地面类合计 {ground} 块 / 共 {ce} 块 CE:false");
    }
}

/// mpp-tris <mod_media> <base_media> <layout> [top_n]
/// Which pieces contributed the TRIANGLES, sorted by how far east they reach.
/// The region box and the triangle gather admit different pieces (links are gated
/// differently), so a tile can be walkable where its box hull says there is nothing —
/// this names the piece responsible.
fn mpp_tris(args: &[String]) {
    use tl2_mikuro_mod_packer_rs::mpp::native;
    use tl2_mikuro_mod_packer_rs::mpp::pipeline::Context;

    if !native::tri_trace_enabled() {
        println!("set MPP_TRI_TRACE=1 to enable the collector");
        return;
    }
    let ctx = Context::new(Path::new(&args[0]), Some(Path::new(&args[1])));
    let lp = Path::new(&args[2]);
    let top: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(14);
    let _ = native::tri_trace_take(); // drop anything stale
    match tl2_mikuro_mod_packer_rs::mpp::compile_mpp(lp, &ctx) {
        Err(e) => println!("{}: {e}", lp.display()),
        Ok(_) => {
            let mut src = native::tri_trace_take();
            println!("{} — {} piece emissions", lp.file_name().unwrap_or_default().to_string_lossy(), src.len());
            src.sort_by(|a, b| b.maxx.partial_cmp(&a.maxx).unwrap_or(std::cmp::Ordering::Equal));
            println!("  furthest EAST (local x):");
            for t in src.iter().take(top) {
                println!(
                    "    maxx {:>8.3}  x[{:>8.3},{:>8.3}] z[{:>8.3},{:>8.3}] {:>5}t{} {} [{}] id={} sc=({:.2},{:.2},{:.2}){}",
                    t.maxx, t.minx, t.maxx, t.minz, t.maxz, t.n_tris,
                    if t.nop { " NOP" } else { "   " },
                    t.piece, t.mesh, t.id, t.scale.0, t.scale.1, t.scale.2,
                    if t.via.is_empty() { String::new() } else { format!(" via {}", t.via) }
                );
            }
        }
    }
}

/// mpp-boxfit <media_dir>
/// CSV of `raw collision hull` against the box GUTS actually shipped
/// (`box_min = -worldExt`, `box_max = box_min + bounds`), one row per tile with a
/// sibling .MPP. Lets the snap rule be FITTED to 1293 observations instead of
/// guessed from one tile — which matters because the rule only shows itself on
/// hulls that land within the pad distance of a 10-unit quantum.
fn mpp_boxfit(args: &[String]) {
    use tl2_mikuro_mod_packer_rs::mpp::layout::load_layout_file;
    use tl2_mikuro_mod_packer_rs::mpp::pipeline::{compute_region, Context};
    use tl2_mikuro_mod_packer_rs::mpp::region::SNAP;

    let media = PathBuf::from(&args[0]);
    let ctx = Context::new(&media, None);
    let root = media.join("LAYOUTS");
    let mut files = Vec::new();
    walk(&root, &mut files);
    let pairs: Vec<(PathBuf, PathBuf)> = files
        .iter()
        .filter(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy().to_uppercase().ends_with(".LAYOUT"))
                .unwrap_or(false)
        })
        .filter_map(|lp| {
            let s = lp.to_string_lossy();
            let mp = PathBuf::from(format!("{}.MPP", &s[..s.len() - ".LAYOUT".len()]));
            mp.exists().then(|| (lp.clone(), mp))
        })
        .collect();

    println!("name,hull_min_x,hull_max_x,hull_min_z,hull_max_z,guts_min_x,guts_max_x,guts_min_z,guts_max_z,our_min_x,our_max_x,our_min_z,our_max_z");
    let rows: Vec<String> = pairs
        .par_iter()
        .filter_map(|(lp, mp)| {
            let layout = load_layout_file(lp).ok()?;
            let buf = std::fs::read(mp).ok()?;
            if buf.len() < 24 {
                return None;
            }
            let gf = |o: usize| f32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
            let (ex, ez, bx, bz) = (gf(8), gf(12), gf(16), gf(20));
            // Unsnapped hull, then our snapped box, so the fit sees both.
            let (raw_box, res) = compute_region(&layout, &ctx, 0.0);
            let (snapped, _) = compute_region(&layout, &ctx, SNAP);
            if res.aabb.null {
                return None;
            }
            let name = lp.strip_prefix(&root).unwrap_or(lp).to_string_lossy().replace(',', ";");
            Some(format!(
                "{name},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6}",
                raw_box.min.x, raw_box.max.x, raw_box.min.z, raw_box.max.z,
                -ex, -ex + bx, -ez, -ez + bz,
                snapped.min.x, snapped.max.x, snapped.min.z, snapped.max.z
            ))
        })
        .collect();
    for r in rows {
        println!("{r}");
    }
}

/// mpp-audit <mod_media> <base_media> [--verbose]
/// Run the pack-time interface health check over a whole tree without packing,
/// and with --verbose dump the per-variant edge coverage table the findings come
/// from (useful for calibrating the thresholds against real data).
fn mpp_audit(args: &[String]) {
    use tl2_mikuro_mod_packer_rs::mpp::audit;
    use tl2_mikuro_mod_packer_rs::mpp::pipeline::Context;

    let media = PathBuf::from(&args[0]);
    let base = PathBuf::from(&args[1]);
    let verbose = args.iter().any(|a| a == "--verbose");
    let t0 = std::time::Instant::now();
    let ctx = Context::new(&media, Some(&base));
    let layouts_root = media.join("LAYOUTS");
    let mut files = Vec::new();
    walk(&layouts_root, &mut files);
    let layouts: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy().to_uppercase().ends_with(".LAYOUT"))
                .unwrap_or(false)
        })
        .collect();
    let owned: Vec<(PathBuf, Vec<u8>)> = layouts
        .par_iter()
        .filter_map(|lp| {
            tl2_mikuro_mod_packer_rs::mpp::compile_mpp(lp, &ctx)
                .ok()
                .map(|d| (lp.clone(), d))
        })
        .collect();
    let baked: Vec<(PathBuf, &[u8])> =
        owned.iter().map(|(p, d)| (p.clone(), d.as_slice())).collect();
    println!(
        "baked {}/{} layouts in {:.1}s",
        owned.len(),
        layouts.len(),
        t0.elapsed().as_secs_f64()
    );

    if verbose {
        // Re-derive the table for the chunk types that have a rules entry.
        let mut seen: Vec<(String, String)> = Vec::new();
        for (lp, _) in &baked {
            if let Ok(rel) = lp.strip_prefix(&layouts_root) {
                let parts: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().to_uppercase())
                    .collect();
                if parts.len() == 3 && !seen.contains(&(parts[0].clone(), parts[1].clone())) {
                    seen.push((parts[0].clone(), parts[1].clone()));
                }
            }
        }
        println!("\nper-variant edge coverage (inside% / spill% of the shared band)");
        for (zone, ctype) in &seen {
            let rules = audit::load_zone_rules(&media, Some(&base), zone);
            let Some((zr, ct)) = audit::find_chunk_type(&rules, ctype) else { continue };
            let fp = zr.footprint(ct);
            let (cx, hw, cz, hd) = fp;
            println!(
                "  {zone}/{ctype}  footprint local x[{:.0},{:.0}] z[{:.0},{:.0}]  exits={:?}",
                cx - hw,
                cx + hw,
                cz - hd,
                cz + hd,
                ct.exits
            );
            let mut rows: Vec<(String, [audit::EdgeCoverage; 4])> = Vec::new();
            for (lp, bytes) in &owned {
                let rel = lp.strip_prefix(&layouts_root).unwrap_or(lp).to_string_lossy().to_uppercase();
                if !rel.starts_with(&format!("{zone}\\{ctype}\\")) {
                    continue;
                }
                if let Some(g) = audit::Grid::parse(bytes) {
                    rows.push((
                        lp.file_name().unwrap_or_default().to_string_lossy().to_string(),
                        g.edge_coverage(fp),
                    ));
                }
            }
            let bdir = base.join("LAYOUTS").join(zone).join(ctype);
            if let Ok(rd) = std::fs::read_dir(&bdir) {
                for e in rd.flatten() {
                    let fname = e.file_name().to_string_lossy().to_string();
                    if !fname.to_uppercase().ends_with(".MPP") {
                        continue;
                    }
                    if let Ok(buf) = std::fs::read(e.path()) {
                        if let Some(g) = audit::Grid::parse(&buf) {
                            rows.push((format!("{fname} (base)"), g.edge_coverage(fp)));
                        }
                    }
                }
            }
            for (name, cov) in rows {
                let s: Vec<String> = (0..4)
                    .map(|i| {
                        format!(
                            "{}: {:>3.0}/{:>3.0}",
                            audit::EDGE_NAMES[i],
                            100.0 * cov[i].inside_frac(),
                            100.0 * cov[i].outside_frac()
                        )
                    })
                    .collect();
                println!("     {:<58} {}", name, s.join("  "));
            }
        }
    }

    let findings = audit::audit_tree(&media, Some(&base), &baked);
    println!("\n== interface findings: {} ==", findings.len());
    for f in &findings {
        println!("  {}/{}/{}\n    {}", f.zone, f.chunk_type, f.variant, f.message);
    }
}

/// mpp-info <mod_media> <base_media> <layout>...
/// Region/header provenance for one tile: the collision hull the grid box comes
/// from, and how many room pieces contributed vs how many had an unresolvable
/// collision mesh. A high `missing` is the signature of a bake that under-fills
/// because the geometry never loaded, which looks identical to "no floor here"
/// in the finished grid.
fn mpp_info(args: &[String]) {
    use tl2_mikuro_mod_packer_rs::mpp::layout::load_layout_file;
    use tl2_mikuro_mod_packer_rs::mpp::pipeline::{generate_header, Context};

    let ctx = Context::new(Path::new(&args[0]), Some(Path::new(&args[1])));
    for lp in args[2..].iter().filter(|a| !a.starts_with("--")) {
        let p = Path::new(lp);
        match load_layout_file(p) {
            Err(e) => println!("{lp}: parse error {e}"),
            Ok(layout) => {
                let h = generate_header(&layout, &ctx);
                println!("\n{}", p.file_name().unwrap_or_default().to_string_lossy());
                println!(
                    "  grid {}x{}   origin=({:.3},{:.3})   snapped box x[{:.2},{:.2}] z[{:.2},{:.2}]",
                    h.grid_w, h.grid_h, h.origin_x, h.origin_z, h.bx.min.x, h.bx.max.x, h.bx.min.z, h.bx.max.z
                );
                println!(
                    "  raw collision hull x[{:.3},{:.3}] z[{:.3},{:.3}]",
                    h.region.aabb.min.x, h.region.aabb.max.x, h.region.aabb.min.z, h.region.aabb.max.z
                );
                println!(
                    "  header worldExt=({:.3},{:.3}) bounds=({:.3},{:.3})",
                    h.world_ext_x, h.world_ext_z, h.bounds_x, h.bounds_z
                );
                println!(
                    "  room pieces contributing collision: {}   with UNRESOLVABLE mesh: {}",
                    h.region.n_pieces_used, h.region.n_pieces_missing
                );
                // --pieces: who is responsible for each hull extremum. A box that
                // disagrees with GUTS by one snap quantum is almost always ONE
                // piece reaching a little too far, so name it.
                if args.iter().any(|a| a == "--pieces") {
                    print_hull_extremes(&layout, &ctx);
                }
                // Sibling shipped .MPP, when there is one: the header is what the
                // engine stamps by, so a delta here misplaces the whole tile and
                // is a different class of problem from a cell disagreement.
                let s = lp.to_uppercase();
                if s.ends_with(".LAYOUT") {
                    let mp = format!("{}.MPP", &lp[..lp.len() - ".LAYOUT".len()]);
                    if let Ok(buf) = std::fs::read(&mp) {
                        if buf.len() >= 24 {
                            let gi = |o: usize| i32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
                            let gf = |o: usize| f32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
                            let (sw, sh) = (gi(0), gi(4));
                            let (sex, sez, sbx, sbz) = (gf(8), gf(12), gf(16), gf(20));
                            println!(
                                "  GUTS   grid {sw}x{sh}   worldExt=({sex:.3},{sez:.3}) bounds=({sbx:.3},{sbz:.3})"
                            );
                            let dw = h.grid_w - sw;
                            let dh = h.grid_h - sh;
                            if (dw, dh) == (0, 0)
                                && h.world_ext_x == sex
                                && h.world_ext_z == sez
                                && h.bounds_x == sbx
                                && h.bounds_z == sbz
                            {
                                println!("  header MATCHES");
                            } else {
                                println!(
                                    "  header DIFFERS  dgrid=({dw:+},{dh:+})  dExt=({:+.3},{:+.3})  dBounds=({:+.3},{:+.3})",
                                    h.world_ext_x - sex,
                                    h.world_ext_z - sez,
                                    h.bounds_x - sbx,
                                    h.bounds_z - sbz
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Cell class for A/B purposes. The three are NOT interchangeable at merge time:
/// the engine stamps a tile's cell over the level grid when `cell != 0xFF`, so
/// void yields to a neighbour while wall overwrites it. A bake that emits wall
/// where GUTS emits void therefore seals its own doorway.
fn cls(v: u8) -> usize {
    match v {
        0 => 0,        // walkable
        0xFF => 2,     // void (no geometry under the cell)
        _ => 1,        // wall
    }
}
const CLS_NAME: [&str; 3] = ["walk", "wall", "void"];

/// Chebyshev distance to the nearest grid border, bucketed.
fn band_of(x: i32, z: i32, gw: i32, gh: i32) -> usize {
    let d = x.min(z).min(gw - 1 - x).min(gh - 1 - z);
    match d {
        0..=2 => 0,
        3..=5 => 1,
        6..=15 => 2,
        _ => 3,
    }
}
const BAND_NAME: [&str; 4] = ["border 0-2", "ring 3-5", "ring 6-15", "interior >15"];

/// Does walkable reach within `k` cells of each side? (order: N=-z, S=+z, W=-x, E=+x)
fn side_touch(g: &[u8], gw: i32, gh: i32, k: i32) -> [bool; 4] {
    let mut t = [false; 4];
    let at = |x: i32, z: i32| g[(z * gw + x) as usize] == 0;
    for x in 0..gw {
        for z in 0..k.min(gh) {
            if at(x, z) {
                t[0] = true;
            }
            if at(x, gh - 1 - z) {
                t[1] = true;
            }
        }
    }
    for z in 0..gh {
        for x in 0..k.min(gw) {
            if at(x, z) {
                t[2] = true;
            }
            if at(gw - 1 - x, z) {
                t[3] = true;
            }
        }
    }
    t
}

/// A/B the offline bake against every GUTS-baked sibling .MPP in a tree.
///
/// One aggregate diff percentage cannot tell "we are 0.3% noisy everywhere" from
/// "we systematically wall the door thresholds", and only the latter can close a
/// tile's interface at merge time. So this reports the full walk/wall/void
/// confusion matrix and repeats it per distance-to-border band.
fn verify_mpp_ab(args: &[String]) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let media = PathBuf::from(&args[0]);
    let sub = args.get(1).cloned().unwrap_or_default();
    let t0 = std::time::Instant::now();
    let ctx = tl2_mikuro_mod_packer_rs::mpp::pipeline::Context::new(&media, None);
    let ctx_s = t0.elapsed().as_secs_f64();

    let root = if sub.is_empty() {
        media.join("LAYOUTS")
    } else {
        media.join("LAYOUTS").join(&sub)
    };
    let mut files = Vec::new();
    walk(&root, &mut files);
    let pairs: Vec<(PathBuf, PathBuf)> = files
        .iter()
        .filter(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy().to_uppercase().ends_with(".LAYOUT"))
                .unwrap_or(false)
        })
        .filter_map(|lp| {
            let s = lp.to_string_lossy();
            let mp = PathBuf::from(format!("{}.MPP", &s[..s.len() - ".LAYOUT".len()]));
            // Windows FS is case-insensitive, so this catches ".mpp" too.
            if mp.exists() {
                Some((lp.clone(), mp))
            } else {
                None
            }
        })
        .collect();
    println!(
        "== MPP A/B vs GUTS ({}) ==\ncontext {ctx_s:.1}s   {} LAYOUT+MPP pairs",
        root.display(),
        pairs.len()
    );

    struct Row {
        name: String,
        i0x: i64,
        dim_ok: bool,
        err: bool,
        conf: [[u64; 3]; 3],
        band: [[[u64; 3]; 3]; 4],
        // walkable-reaches-side, GUTS vs ours, within 3 cells of each side
        side_lost: [bool; 4],
    }

    let done = AtomicUsize::new(0);
    let total = pairs.len();
    let t1 = std::time::Instant::now();
    let rows: Vec<Row> = pairs
        .par_iter()
        .map(|(lp, mp)| {
            let name = lp
                .strip_prefix(&root)
                .unwrap_or(lp)
                .to_string_lossy()
                .to_string();
            let mut r = Row {
                name,
                i0x: 0,
                dim_ok: false,
                err: false,
                conf: [[0; 3]; 3],
                band: [[[0; 3]; 3]; 4],
                side_lost: [false; 4],
            };
            let shipped = std::fs::read(mp).unwrap_or_default();
            let gen = match tl2_mikuro_mod_packer_rs::mpp::compile_mpp(lp, &ctx) {
                Ok(g) => g,
                Err(_) => {
                    r.err = true;
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if d.is_multiple_of(200) || d == total {
                        eprint!("\r  {d}/{total}   ");
                    }
                    return r;
                }
            };
            if shipped.len() < 24 || gen.len() != shipped.len() || gen[..8] != shipped[..8] {
                let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                if d.is_multiple_of(200) || d == total {
                    eprint!("\r  {d}/{total}   ");
                }
                return r;
            }
            r.dim_ok = true;
            r.i0x = tl2_mikuro_mod_packer_rs::mpp::audit::stamp_i0(
                f32::from_le_bytes(gen[8..12].try_into().unwrap()));
            let gw = i32::from_le_bytes(gen[0..4].try_into().unwrap());
            let gh = i32::from_le_bytes(gen[4..8].try_into().unwrap());
            let (a, b) = (&shipped[24..], &gen[24..]);
            for z in 0..gh {
                for x in 0..gw {
                    let i = (z * gw + x) as usize;
                    let (ca, cb) = (cls(a[i]), cls(b[i]));
                    r.conf[ca][cb] += 1;
                    r.band[band_of(x, z, gw, gh)][ca][cb] += 1;
                }
            }
            let sa = side_touch(a, gw, gh, 3);
            let sb = side_touch(b, gw, gh, 3);
            for s in 0..4 {
                r.side_lost[s] = sa[s] && !sb[s];
            }
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            if d.is_multiple_of(200) || d == total {
                eprint!("\r  {d}/{total}   ");
            }
            r
        })
        .collect();
    eprintln!();

    let n = rows.len();
    let errs = rows.iter().filter(|r| r.err).count();
    let dim_bad = rows.iter().filter(|r| !r.dim_ok && !r.err).count();
    let ok: Vec<&Row> = rows.iter().filter(|r| r.dim_ok).collect();
    let mut conf = [[0u64; 3]; 3];
    let mut band = [[[0u64; 3]; 3]; 4];
    for r in &ok {
        for (i, row) in r.conf.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                conf[i][j] += v;
            }
        }
        for (b, rb) in r.band.iter().enumerate() {
            for (i, row) in rb.iter().enumerate() {
                for (j, v) in row.iter().enumerate() {
                    band[b][i][j] += v;
                }
            }
        }
    }
    let cells: u64 = conf.iter().flatten().sum();
    let diff: u64 = (0..3).map(|i| (0..3).filter(|&j| j != i).map(|j| conf[i][j]).sum::<u64>()).sum();
    println!(
        "compile+compare {:.1}s   dim-match {}/{n}   dim-mismatch {dim_bad}   errors {errs}",
        t1.elapsed().as_secs_f64(),
        ok.len()
    );
    println!(
        "cells {cells}   diff {diff}   accuracy {:.4}%",
        100.0 * (cells - diff) as f64 / cells.max(1) as f64
    );

    let show = |label: &str, m: &[[u64; 3]; 3]| {
        let tot: u64 = m.iter().flatten().sum();
        println!("\n-- {label} ({tot} cells) --");
        println!("            ours=walk      ours=wall      ours=void");
        for (i, row) in m.iter().enumerate() {
            print!("GUTS={:<5}", CLS_NAME[i]);
            for v in row {
                let pct = 100.0 * *v as f64 / tot.max(1) as f64;
                print!("{v:>12} {pct:>5.2}%");
            }
            println!();
        }
    };
    show("ALL CELLS", &conf);
    for b in 0..4 {
        show(BAND_NAME[b], &band[b]);
    }

    // The connectivity-relevant asymmetries, called out by name.
    println!("\n-- interface risk (blocking direction only) --");
    for (lbl, i, j) in [
        ("GUTS void -> ours WALL  (stamps a wall where engine expected yield)", 2usize, 1usize),
        ("GUTS walk -> ours WALL  (under-fill: walkable sealed)", 0, 1),
        ("GUTS walk -> ours void  (under-fill: geometry missed)", 0, 2),
        ("GUTS wall -> ours walk  (over-fill, opens)", 1, 0),
        ("GUTS void -> ours walk  (over-fill, opens)", 2, 0),
    ] {
        println!(
            "  {lbl}: {} total, border 0-2 {} ({:.1}% of that band's diff)",
            conf[i][j],
            band[0][i][j],
            100.0 * band[0][i][j] as f64
                / (0..3)
                    .map(|a| (0..3).filter(|&b2| b2 != a).map(|b2| band[0][a][b2]).sum::<u64>())
                    .sum::<u64>()
                    .max(1) as f64
        );
    }

    // A wrong grid dim is not a cosmetic miss: the engine stamps by
    // floor((pos - worldExt)/cell), so a dim/header disagreement misplaces the
    // whole tile. List them by name rather than as a count.
    let bad: Vec<&Row> = rows.iter().filter(|r| !r.dim_ok && !r.err).collect();
    if !bad.is_empty() {
        println!("\n-- dim/header mismatches (tile would stamp at the wrong size/offset) --");
        for r in bad.iter().take(40) {
            println!("  {}", r.name);
        }
    }

    // Split accuracy by the stamp-alignment index. If WE placed content differently
    // from GUTS on i0==1 tiles, their cell agreement would collapse; if it matches the
    // i0==2 tiles, GUTS bakes the same in-grid placement and the runtime shift is the
    // engine's own reading, not ours.
    println!("
-- cell agreement split by stamp-alignment index i0 (X axis) --");
    for want in [1i64, 2] {
        let sel: Vec<&&Row> = ok.iter().filter(|r| r.i0x == want).collect();
        if sel.is_empty() {
            continue;
        }
        let cells: u64 = sel.iter().map(|r| r.conf.iter().flatten().sum::<u64>()).sum();
        let diff: u64 = sel
            .iter()
            .map(|r| {
                (0..3)
                    .map(|i| (0..3).filter(|&j| j != i).map(|j| r.conf[i][j]).sum::<u64>())
                    .sum::<u64>()
            })
            .sum();
        println!(
            "  i0={want}: {:>4} tiles  {:>11} cells  diff {:>8}  accuracy {:.4}%",
            sel.len(),
            cells,
            diff,
            100.0 * (cells - diff) as f64 / cells.max(1) as f64
        );
    }

    let lost: Vec<&&Row> = ok.iter().filter(|r| r.side_lost.iter().any(|&b| b)).collect();
    println!(
        "\n-- tiles where a side GUTS could reach (walkable within 3 cells) is no longer reachable in ours: {}/{} --",
        lost.len(),
        ok.len()
    );
    for r in lost.iter().take(25) {
        let s: Vec<&str> = ["N", "S", "W", "E"]
            .iter()
            .zip(r.side_lost)
            .filter(|(_, l)| *l)
            .map(|(n, _)| *n)
            .collect();
        println!("  {} lost {:?}", r.name, s);
    }

    let mut worst: Vec<&&Row> = ok.iter().collect();
    worst.sort_by_key(|r| std::cmp::Reverse(r.band[0][2][1] + r.band[0][0][1] + r.band[0][0][2]));
    println!("\n-- worst border-band blockers (void->wall + walk->wall + walk->void, band 0-2) --");
    for r in worst.iter().take(15) {
        println!(
            "  {:>6}  v>w {:<5} w>W {:<5} w>v {:<5}  {}",
            r.band[0][2][1] + r.band[0][0][1] + r.band[0][0][2],
            r.band[0][2][1],
            r.band[0][0][1],
            r.band[0][0][2],
            r.name
        );
    }
}

/// mpp-gen <mod_media> <base_media> <out_dir> [rel_substring_filter]
/// Context built ONCE (mod-over-base overlay: mod_media wins, base_media fallback);
/// walk mod_media/LAYOUTS, compile every .LAYOUT, write out_dir/<rel>.MPP. Parallel.
/// The optional filter (case-insensitive substring of the path relative to
/// LAYOUTS/) narrows the run to one tile or folder for diagnosis.
fn mpp_gen(args: &[String]) {
    let mod_media = PathBuf::from(&args[0]);
    let base_media = PathBuf::from(&args[1]);
    let out_dir = PathBuf::from(&args[2]);
    let filter = args.get(3).map(|s| s.to_uppercase());
    let t0 = std::time::Instant::now();
    let ctx = tl2_mikuro_mod_packer_rs::mpp::pipeline::Context::new(&mod_media, Some(&base_media));
    let _ = tl2_mikuro_mod_packer_rs::mpp::native::link_miss_report(); // drop anything stale
    let layouts_root = mod_media.join("LAYOUTS");
    let mut files = Vec::new();
    walk(&layouts_root, &mut files);
    let layouts: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy().to_uppercase().ends_with(".LAYOUT"))
                .unwrap_or(false)
        })
        .filter(|p| match &filter {
            None => true,
            Some(f) => p
                .strip_prefix(&layouts_root)
                .unwrap_or(p)
                .to_string_lossy()
                .to_uppercase()
                .contains(f),
        })
        .collect();
    let ctx_s = t0.elapsed().as_secs_f64();
    let t1 = std::time::Instant::now();
    let (ok, fail): (Vec<_>, Vec<_>) = layouts
        .par_iter()
        .map(|lp| {
            let rel = lp.strip_prefix(&layouts_root).unwrap_or(lp);
            let rels = rel.to_string_lossy();
            let out = out_dir.join(format!("{}.MPP", &rels[..rels.len() - ".LAYOUT".len()]));
            match tl2_mikuro_mod_packer_rs::mpp::compile_mpp(lp, &ctx) {
                Ok(data) => {
                    if let Some(d) = out.parent() {
                        std::fs::create_dir_all(d).ok();
                    }
                    std::fs::write(&out, &data).ok();
                    Ok(())
                }
                Err(e) => Err(format!("{}: {e}", rels)),
            }
        })
        .partition(Result::is_ok);
    println!(
        "mpp-gen: {} layouts, ctx {:.1}s, compile {:.1}s, ok={} fail={}",
        layouts.len(),
        ctx_s,
        t1.elapsed().as_secs_f64(),
        ok.len(),
        fail.len()
    );
    for e in fail.iter().filter_map(|r| r.as_ref().err()).take(20) {
        println!("  FAIL {e}");
    }
    // Same report the packer prints: an unresolved link drops its geometry from
    // the grid without failing the compile, which is how a base tree missing its
    // `.LAYOUT` files silently cost one tile 5010 cells of walkable ground.
    let misses = tl2_mikuro_mod_packer_rs::mpp::native::link_miss_report();
    if !misses.is_empty() {
        let total: usize = misses.iter().map(|(_, n)| n).sum();
        println!(
            "  WARNING: {total} Layout Link(s) across {} target(s) did not resolve — \
             their geometry is MISSING from these grids",
            misses.len()
        );
        for (f, n) in misses.iter().take(12) {
            println!("           {n:>5} x {f}");
        }
        if misses.len() > 12 {
            println!("           ... and {} more target(s)", misses.len() - 12);
        }
    }
}

fn verify_mpp(args: &[String]) {
    let media = PathBuf::from(&args[0]);
    let ctx = tl2_mikuro_mod_packer_rs::mpp::pipeline::Context::new(&media, None);
    for lp in &args[1..] {
        let t0 = std::time::Instant::now();
        match tl2_mikuro_mod_packer_rs::mpp::compile_mpp(Path::new(lp), &ctx) {
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
