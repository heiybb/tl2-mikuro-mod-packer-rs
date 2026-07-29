//! tl2-mikuro-mod-packer — Torchlight 2 .MOD builder CLI (Rust port of mikuro_mod_packer).
//!
//! Usage: tl2-mikuro-mod-packer [--in-place|--temp-copy] [--mpp {re,none}]
//!                   [--raw {auto,none}] [--deploy] <mod_directory>
//!
//! Extra single-file tools:
//!   tl2-mikuro-mod-packer compile-dat <in.DAT> <out.BINDAT>
//!   tl2-mikuro-mod-packer compile-layout <in.LAYOUT> <out.BINLAYOUT>
//!   tl2-mikuro-mod-packer compile-mpp <in.LAYOUT> <out.mpp> [--media <MEDIA>]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use tl2_mikuro_mod_packer_rs::packer::{
    self, convert_all, pack_mod, read_mod_metadata, tl2_mods_dir, Overrides,
};

mod base_bundle;
mod tui;

fn usage() -> ExitCode {
    eprintln!("Usage: tl2-mikuro-mod-packer [--in-place|--temp-copy] [--mpp {{re,none}}] [--raw {{auto,none}}] [--deploy] <mod_directory>");
    eprintln!("  (no flag)    Compile in memory — nothing but the .MOD is written (default)");
    eprintln!("  --in-place   ALSO leave the compiled .BINDAT/.BINLAYOUT in your MEDIA tree");
    eprintln!("  --temp-copy  Compile on disk in a throwaway copy; your MEDIA stays clean");
    eprintln!("  --deploy     Output straight to TL2's default mods dir as <MOD_FILE_NAME>.MOD");
    eprintln!("  --mpp re     Offline .MPP generator (DEFAULT; robust, ~99.7% cell-accurate)");
    eprintln!("  --mpp none   Skip generation, keep the source's existing .MPP");
    eprintln!("  --raw auto   Emit RAW indexes on-demand per content type (DEFAULT)");
    eprintln!("  --raw none   Skip RAW generation entirely");
    eprintln!();
    eprintln!("Unpack (the inverse — container back to editable sources):");
    eprintln!("  tl2-mikuro-mod-packer unpack <in.MOD|DATA.PAK> <out_dir> [--raw] [--no-times] [--filter <text>]");
    eprintln!("    --raw        Write the container payloads verbatim (.DAT stays BINDAT)");
    eprintln!("    --no-times   Do not restore file times from the manifest");
    eprintln!("    --filter T   Only entries whose path contains T (case-insensitive)");
    eprintln!();
    eprintln!("Single-file tools:");
    eprintln!("  tl2-mikuro-mod-packer compile-dat <in.DAT> <out.BINDAT>");
    eprintln!("  tl2-mikuro-mod-packer compile-layout <in.LAYOUT> <out.BINLAYOUT>");
    eprintln!("  tl2-mikuro-mod-packer compile-mpp <in.LAYOUT> <out.mpp> [--media <MEDIA>]");
    eprintln!("  tl2-mikuro-mod-packer decompile-dat <in.BINDAT> <out.DAT>");
    eprintln!("  tl2-mikuro-mod-packer decompile-layout <in.BINLAYOUT> <out.LAYOUT>");
    ExitCode::FAILURE
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

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // No arguments (double-clicked / dropped into a folder and run) → interactive
    // TUI. Any argument keeps the existing scriptable CLI untouched. Guard on a
    // real terminal so a bare run under a pipe / CI prints usage instead of
    // hanging in the event loop (the diagnostic list dump is still allowed).
    if argv.is_empty() {
        use std::io::IsTerminal;
        let interactive = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
        if interactive || std::env::var_os("TL2_MODPACK_LIST").is_some() {
            return tui::run();
        }
        return usage();
    }

    // single-file subcommands
    match argv.first().map(String::as_str) {
        Some("compile-dat") if argv.len() >= 3 => {
            // Optional: --global-dict <bindat_string_dict.bin> compiles with the
            // GLOBAL corpus string-id scheme (exact GUTS sub_10289A40 output)
            // instead of per-file hash ids.
            let mut dict_path: Option<String> = None;
            let mut rest = argv[3..].iter();
            while let Some(a) = rest.next() {
                if a == "--global-dict" {
                    dict_path = rest.next().cloned();
                }
            }
            let res = match dict_path {
                Some(dp) => tl2_mikuro_mod_packer_rs::bindat::CorpusDict::load(Path::new(&dp))
                    .and_then(|d| {
                        tl2_mikuro_mod_packer_rs::bindat::compile_dat_file_corpus(Path::new(&argv[1]), &d)
                    }),
                None => tl2_mikuro_mod_packer_rs::bindat::compile_dat_file(Path::new(&argv[1])),
            };
            return match res {
                Ok(data) => {
                    std::fs::write(&argv[2], &data).expect("write output");
                    println!("wrote {} bytes", data.len());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some("compile-layout") if argv.len() >= 3 => {
            return match tl2_mikuro_mod_packer_rs::binlayout::compile_layout_file(Path::new(&argv[1])) {
                Ok(data) => {
                    std::fs::write(&argv[2], &data).expect("write output");
                    println!("wrote {} bytes", data.len());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some("decompile-dat") if argv.len() >= 3 => {
            // decompile-dat <in.BINDAT> <out.DAT> [--utf8] — inverse of
            // compile-dat. Keys/section names come from the embedded reverse
            // dictionary (UNK_<HEX8> for a mod's new keys, which compiles back
            // to the same hash), values verbatim — so the default output is a
            // UTF-16LE source that recompiles byte-for-byte. --utf8 writes the
            // old plain-text dump instead, for eyeballing and diffing.
            use tl2_mikuro_mod_packer_rs::bindat_decompile as bdd;
            let utf8 = argv[3..].iter().any(|a| a == "--utf8");
            return match std::fs::read(&argv[1])
                .map_err(|e| e.to_string())
                .and_then(|d| bdd::decompile_bindat_reported(&d))
            {
                Ok(d) => {
                    let bytes = if utf8 {
                        d.text.into_bytes()
                    } else {
                        bdd::to_dat_bytes(&d.text)
                    };
                    std::fs::write(&argv[2], bytes).expect("write output");
                    println!("decompiled → {}", argv[2]);
                    if !d.recovered_names.is_empty() {
                        println!(
                            "  recovered {} name(s) beyond the shipped key table: {}",
                            d.recovered_names.len(),
                            tl2_mikuro_mod_packer_rs::dat_keys::paint_ok(
                                &d.recovered_names.iter().take(6).cloned().collect::<Vec<_>>().join(" "),
                                std::io::IsTerminal::is_terminal(&std::io::stdout()),
                            )
                        );
                    }
                    if !d.unknown_names.is_empty() {
                        println!(
                            "  note: {} name(s) had no dictionary entry and kept a raw-hash \
                             placeholder (do NOT edit the digits): {}",
                            d.unknown_names.len(),
                            tl2_mikuro_mod_packer_rs::dat_keys::paint_warn(
                                &d.unknown_names.iter().take(8).cloned().collect::<Vec<_>>().join(" "),
                                std::io::IsTerminal::is_terminal(&std::io::stdout()),
                            )
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some("decompile-layout") if argv.len() >= 3 => {
            // decompile-layout <in.BINLAYOUT> <out.LAYOUT> — inverse of
            // compile-layout. Fully recoverable (schema-driven), so the output
            // is a real re-editable source, not just a dump.
            use tl2_mikuro_mod_packer_rs::binlayout_decompile as bld;
            return match std::fs::read(&argv[1])
                .map_err(|e| e.to_string())
                .and_then(|d| bld::decompile_binlayout(&d))
            {
                Ok(text) => {
                    std::fs::write(&argv[2], bld::to_layout_bytes(&text)).expect("write output");
                    println!("decompiled → {}", argv[2]);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some("unpack") if argv.len() >= 3 => {
            return run_unpack(&argv[1], &argv[2], &argv[3..]);
        }
        Some("unpack-base") if argv.len() >= 2 => {
            // unpack-base <out_dir> — unpack the embedded base bundle (no game
            // install needed).
            return match base_bundle::ensure_extracted(Path::new(&argv[1])) {
                Ok(()) => {
                    println!("unpacked embedded base bundle → {}", argv[1]);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some("extract-base") if argv.len() >= 3 => {
            // extract-base <DATA.PAK> <out_dir> — decompile base LEVELSETS DATs +
            // extract their collision meshes for offline MPP (TL2_MEDIA_DIR).
            return match tl2_mikuro_mod_packer_rs::pak::PakReader::open_pak(Path::new(&argv[1]))
                .and_then(|mut r| r.extract_base(Path::new(&argv[2])))
            {
                Ok((d, m)) => {
                    println!("extracted {d} base DATs + {m} collision meshes → {}", argv[2]);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some("compile-mpp") if argv.len() >= 3 => {
            let mut media = packer::tl2_media_dir();
            let mut fallback: Option<PathBuf> = None;
            let mut rest = argv[3..].iter();
            while let Some(a) = rest.next() {
                if a == "--media" {
                    if let Some(m) = rest.next() {
                        media = PathBuf::from(m);
                    }
                } else if a == "--fallback" {
                    // base-install MEDIA for the mod-over-base VFS overlay, so a
                    // single mod tile resolves base-game link/mesh targets exactly
                    // as a full mod build does (Context in generate_mpp_files).
                    if let Some(m) = rest.next() {
                        fallback = Some(PathBuf::from(m));
                    }
                }
            }
            let ctx = tl2_mikuro_mod_packer_rs::mpp::pipeline::Context::new(
                &media,
                fallback.as_deref(),
            );
            return match tl2_mikuro_mod_packer_rs::mpp::compile_mpp(Path::new(&argv[1]), &ctx) {
                Ok(data) => {
                    std::fs::write(&argv[2], &data).expect("write output");
                    println!("wrote {} bytes", data.len());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        _ => {}
    }

    // flag parsing (mirrors the Python CLI)
    let mut in_place = false;
    let mut temp_copy = false;
    let mut deploy = false;
    let mut mpp_backend = "re".to_string();
    let mut raw_mode = "auto".to_string();
    let mut positional: Vec<String> = Vec::new();
    let mut it = argv.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--in-place" => in_place = true,
            "--temp-copy" => temp_copy = true,
            "--deploy" => deploy = true,
            "--mpp" => mpp_backend = it.next().unwrap_or_else(|| "re".into()),
            "--raw" => raw_mode = it.next().unwrap_or_else(|| "auto".into()),
            _ if a.starts_with("--mpp=") => mpp_backend = a[6..].to_string(),
            _ if a.starts_with("--raw=") => raw_mode = a[6..].to_string(),
            _ => positional.push(a),
        }
    }
    if !matches!(mpp_backend.as_str(), "re" | "none") {
        eprintln!("Error: --mpp must be one of re|none (got {mpp_backend:?})");
        return ExitCode::FAILURE;
    }
    if !matches!(raw_mode.as_str(), "auto" | "none") {
        eprintln!("Error: --raw must be one of auto|none (got {raw_mode:?})");
        return ExitCode::FAILURE;
    }
    let Some(mod_arg) = positional.first() else {
        return usage();
    };

    let mod_dir = std::fs::canonicalize(mod_arg)
        .unwrap_or_else(|_| PathBuf::from(mod_arg));
    let media_dir = mod_dir.join("MEDIA");
    if !media_dir.is_dir() {
        eprintln!("Error: no MEDIA directory in {}", mod_dir.display());
        return ExitCode::FAILURE;
    }

    let meta = read_mod_metadata(&mod_dir);
    println!("MOD: {}", meta.name);
    println!("Title: {}", meta.title);

    let t0 = std::time::Instant::now();
    let mut overrides: Option<Overrides> = None;
    let mut work_media = media_dir.clone();
    let results;
    if in_place {
        println!("[1/3] Converting files in-place...");
        results = convert_all(&media_dir, None, &mpp_backend, &raw_mode);
    } else if temp_copy {
        let tmp_media = std::env::temp_dir().join(format!(
            "mikuro_mod_{}",
            mod_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        ));
        if tmp_media.exists() {
            std::fs::remove_dir_all(&tmp_media).expect("clean temp media");
        }
        println!("[1/3] Copying MEDIA to temp + converting...");
        copy_tree(&media_dir, &tmp_media).expect("copy MEDIA");
        results = convert_all(&tmp_media, None, &mpp_backend, &raw_mode);
        work_media = tmp_media;
    } else {
        println!("[1/3] Converting in memory (no copy)...");
        let mut ov = Overrides::new();
        results = convert_all(&media_dir, Some(&mut ov), &mpp_backend, &raw_mode);
        overrides = Some(ov);
    }

    println!("\n  Results:");
    println!("    DAT→BINDAT:   {} files", results.bindat);
    println!("    LAYOUT→BINLAYOUT: {} files", results.binlayout);
    println!("    RAW generated: {} files", results.raw);
    println!("    MPP generated: {} files (backend={mpp_backend})", results.mpp);
    if !results.errors.is_empty() {
        println!("    Errors: {}", results.errors.len());
        for e in results.errors.iter().take(5) {
            println!("      {e}");
        }
    }
    let convert_secs = t0.elapsed().as_secs_f64();
    println!("    (convert took {convert_secs:.1}s)");

    let output_path = if deploy {
        let mods_dir = tl2_mods_dir();
        std::fs::create_dir_all(&mods_dir).expect("create mods dir");
        mods_dir.join(format!("{}.MOD", meta.name))
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        mod_dir.join(format!("{}_{}.MOD", meta.name, timestamp_local(now)))
    };

    println!("\n[2/3] Packing container...");
    let t_pack = std::time::Instant::now();
    let mod_size = pack_mod(&work_media, &output_path, Some(&mod_dir), overrides.as_ref())
        .expect("pack .MOD");
    let pack_secs = t_pack.elapsed().as_secs_f64();

    if temp_copy {
        let _ = std::fs::remove_dir_all(&work_media);
    }
    println!(
        "\n[3/3] Done in {:.1}s  (convert {convert_secs:.1}s + pack {pack_secs:.1}s)",
        t0.elapsed().as_secs_f64()
    );
    println!("  Output: {}", output_path.display());
    println!("  Size:   {:.2} MB  ({mod_size} bytes)", mod_size as f64 / 1.0e6);
    ExitCode::SUCCESS
}

/// `unpack <in.MOD|DATA.PAK> <out_dir> [--raw] [--no-times] [--filter T]`.
///
/// A `DATA.PAK` is recognised by name (its manifest lives in the sibling
/// `.PAK.MAN` and it carries no mod header); anything else is read as a `.MOD`.
fn run_unpack(input: &str, out_dir: &str, flags: &[String]) -> ExitCode {
    use tl2_mikuro_mod_packer_rs::unpack::{unpack_mod_with_progress, unpack_pak, Options};

    let mut opt = Options::default();
    let mut it = flags.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--raw" => opt.raw = true,
            "--no-times" => opt.restore_times = false,
            "--filter" => opt.filter = it.next().cloned(),
            other => {
                eprintln!("Error: unknown unpack flag {other:?}");
                return ExitCode::FAILURE;
            }
        }
    }

    let src = Path::new(input);
    let dst = Path::new(out_dir);
    if !src.is_file() {
        eprintln!("Error: no such file: {input}");
        return ExitCode::FAILURE;
    }
    let is_pak = src
        .file_name()
        .map(|n| n.to_string_lossy().to_uppercase().ends_with(".PAK"))
        .unwrap_or(false);

    // Progress: files are wildly uneven in size (a 40 MB mesh next to a 200 byte
    // DAT), so tick on a timer rather than every Nth file. Extraction is
    // parallel, so the throttle state lives behind a mutex — which also keeps
    // two workers from interleaving a line.
    let t0 = std::time::Instant::now();
    let last = std::sync::Mutex::new(std::time::Instant::now());
    let tick = |done: usize, total: usize| {
        let Ok(mut last) = last.lock() else { return };
        if done == total || last.elapsed().as_millis() >= 500 {
            *last = std::time::Instant::now();
            let pct = if total > 0 { done * 100 / total } else { 100 };
            print!("\r  {done}/{total} files ({pct}%)   ");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    };

    println!("Unpacking {input} → {out_dir}");
    let res = if is_pak {
        unpack_pak(src, dst, &opt, &tick)
    } else {
        unpack_mod_with_progress(src, dst, &opt, &tick)
    };
    println!();
    match res {
        Ok(rep) => {
            println!(
                "  {} files, {:.2} MB  ({:.1}s)",
                rep.files,
                rep.bytes as f64 / 1.0e6,
                t0.elapsed().as_secs_f64()
            );
            println!("    BINDAT→DAT:       {} files", rep.dats);
            println!("    BINLAYOUT→LAYOUT: {} files", rep.layouts);
            println!("    copied verbatim:  {} files", rep.copied);
            if !rep.recovered_names.is_empty() {
                let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
                println!(
                    "  {} name(s) were NOT in the shipped key table and were recovered                      from the extended dictionary or the file's own context:",
                    rep.recovered_names.len()
                );
                for n in rep.recovered_names.iter().take(10) {
                    println!("      {}", tl2_mikuro_mod_packer_rs::dat_keys::paint_ok(n, tty));
                }
                if rep.recovered_names.len() > 10 {
                    println!("      … and {} more", rep.recovered_names.len() - 10);
                }
            }
            if !rep.unknown_names.is_empty() {
                println!(
                    "  {} DAT name(s) had no dictionary entry and kept a raw-hash \
                     placeholder — they still recompile to the same hash, but do NOT \
                     edit the digits:",
                    rep.unknown_names.len()
                );
                let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
                for n in rep.unknown_names.iter().take(10) {
                    println!("      {}", tl2_mikuro_mod_packer_rs::dat_keys::paint_warn(n, tty));
                }
                if rep.unknown_names.len() > 10 {
                    println!("      … and {} more", rep.unknown_names.len() - 10);
                }
            }
            if !rep.generated.is_empty() {
                println!(
                    "  {} file(s) were GENERATED by the original pack (index rebuilds, \
                     no source file) and are rebuilt on every pack: {}",
                    rep.generated.len(),
                    rep.generated
                        .iter()
                        .take(8)
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
            for w in &rep.warnings {
                println!("  note: {w}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// YYYYMMDD_HHMMSS, LOCAL time (mirrors datetime.now()).
#[allow(unused_variables)]
fn timestamp_local(unix_secs: u64) -> String {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::SystemInformation::GetLocalTime;
        let mut st = std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>();
        GetLocalTime(&mut st);
        return format!(
            "{:04}{:02}{:02}_{:02}{:02}{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        );
    }
    #[allow(unreachable_code)]
    civil_utc(unix_secs)
}

/// Fallback: UTC timestamp (collision-free unique name is all this needs).
fn civil_utc(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs = unix_secs % 86_400;
    // civil_from_days (Howard Hinnant)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}
