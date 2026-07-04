//! mikuro-mod — Torchlight 2 .MOD builder CLI (Rust port of mikuro_mod_packer).
//!
//! Usage: mikuro-mod [--in-place|--temp-copy] [--mpp {re,dll,none}]
//!                   [--raw {auto,none}] [--deploy] <mod_directory>
//!
//! Extra single-file tools:
//!   mikuro-mod compile-dat <in.DAT> <out.BINDAT>
//!   mikuro-mod compile-layout <in.LAYOUT> <out.BINLAYOUT>
//!   mikuro-mod compile-mpp <in.LAYOUT> <out.mpp> [--media <MEDIA>]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mikuro_mod_rs::packer::{
    self, convert_all, pack_mod, read_mod_metadata, tl2_mods_dir, Overrides,
};

fn usage() -> ExitCode {
    eprintln!("Usage: mikuro-mod [--in-place|--temp-copy] [--mpp {{re,dll,none}}] [--raw {{auto,none}}] [--deploy] <mod_directory>");
    eprintln!("  (default)    Convert in memory, pack without copying MEDIA (fastest, non-mutating)");
    eprintln!("  --in-place   Write converted files into MEDIA, then pack");
    eprintln!("  --temp-copy  Copy MEDIA to a temp dir, convert there, then pack");
    eprintln!("  --deploy     Output straight to TL2's default mods dir as <MOD_FILE_NAME>.MOD");
    eprintln!("  --mpp re     Offline .MPP generator (DEFAULT; robust, ~99.7% cell-accurate)");
    eprintln!("  --mpp dll    Byte-exact .MPP via the real EditorGuts.dll (needs editor env)");
    eprintln!("  --mpp none   Skip generation, keep the source's existing .MPP");
    eprintln!("  --raw auto   Emit RAW indexes on-demand per content type (DEFAULT)");
    eprintln!("  --raw none   Skip RAW generation entirely");
    eprintln!();
    eprintln!("Single-file tools:");
    eprintln!("  mikuro-mod compile-dat <in.DAT> <out.BINDAT>");
    eprintln!("  mikuro-mod compile-layout <in.LAYOUT> <out.BINLAYOUT>");
    eprintln!("  mikuro-mod compile-mpp <in.LAYOUT> <out.mpp> [--media <MEDIA>]");
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
                Some(dp) => mikuro_mod_rs::bindat::CorpusDict::load(Path::new(&dp))
                    .and_then(|d| {
                        mikuro_mod_rs::bindat::compile_dat_file_corpus(Path::new(&argv[1]), &d)
                    }),
                None => mikuro_mod_rs::bindat::compile_dat_file(Path::new(&argv[1])),
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
            return match mikuro_mod_rs::binlayout::compile_layout_file(Path::new(&argv[1])) {
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
        Some("compile-mpp") if argv.len() >= 3 => {
            let mut media = packer::tl2_media_dir();
            let mut rest = argv[3..].iter();
            while let Some(a) = rest.next() {
                if a == "--media" {
                    if let Some(m) = rest.next() {
                        media = PathBuf::from(m);
                    }
                }
            }
            let ctx = mikuro_mod_rs::mpp::pipeline::Context::new(&media, None);
            return match mikuro_mod_rs::mpp::compile_mpp(Path::new(&argv[1]), &ctx) {
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
    if !matches!(mpp_backend.as_str(), "re" | "dll" | "none") {
        eprintln!("Error: --mpp must be one of re|dll|none (got {mpp_backend:?})");
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

    println!("\n[2/3] Packing MOD...");
    let mod_size = pack_mod(&work_media, &output_path, Some(&mod_dir), overrides.as_ref())
        .expect("pack .MOD");

    if temp_copy {
        let _ = std::fs::remove_dir_all(&work_media);
    }
    println!("\n[3/3] Done! ({:.1}s)", t0.elapsed().as_secs_f64());
    println!("  Output: {}", output_path.display());
    println!("  Size:   {mod_size} bytes");
    ExitCode::SUCCESS
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
