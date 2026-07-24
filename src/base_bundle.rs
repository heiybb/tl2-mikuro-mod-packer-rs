//! Embedded base-data bundle — the always-available bottom tier of the base
//! resolution (loose MEDIA > DATA.PAK > this). Lets the packer regenerate level
//! pathing and BASEFILE-inherited RAW indexes on a machine with no Torchlight 2
//! install at all (e.g. the exe dropped into a mod-source folder elsewhere).
//!
//! Contents = the same set `PakReader::extract_base` produces (base LEVELSETS +
//! UNITS DATs decompiled to text, plus the collision meshes), so a pack against
//! it is byte-identical to one against loose MEDIA. Format: gzip of
//! `"TL2BUND1"` + `u32 count` + per file `{ u16 pathlen, path (utf8, MEDIA-rel,
//! '/'), u32 len, data }`. ~3 MB compressed, ~26 MB unpacked.

use std::path::Path;

const BUNDLE: &[u8] = include_bytes!("../data/base_bundle.bin");

/// Ensure the embedded base bundle is unpacked under `cache_dir` (reused if a
/// previous run — from here or DATA.PAK — already populated it).
pub fn ensure_extracted(cache_dir: &Path) -> Result<(), String> {
    if cache_dir.join("LEVELSETS").is_dir() {
        return Ok(());
    }
    // gzip trailer ISIZE (last 4 bytes) = uncompressed size mod 2^32; the bundle
    // is ~26 MB so it is exact.
    if BUNDLE.len() < 8 {
        return Err("embedded base bundle is truncated".into());
    }
    let isize = u32::from_le_bytes(BUNDLE[BUNDLE.len() - 4..].try_into().unwrap()) as usize;
    let mut raw = vec![0u8; isize];
    let n = libdeflater::Decompressor::new()
        .gzip_decompress(BUNDLE, &mut raw)
        .map_err(|e| format!("base bundle inflate: {e:?}"))?;
    raw.truncate(n);

    if raw.get(..8) != Some(b"TL2BUND1") {
        return Err("base bundle: bad magic".into());
    }
    let mut o = 8usize;
    let rd_u32 = |b: &[u8], o: &mut usize| -> Result<u32, String> {
        let s = b.get(*o..*o + 4).ok_or("base bundle: eof u32")?;
        *o += 4;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };
    let rd_u16 = |b: &[u8], o: &mut usize| -> Result<u16, String> {
        let s = b.get(*o..*o + 2).ok_or("base bundle: eof u16")?;
        *o += 2;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    };
    // Parse the container into (dst, data) pairs first, then write in parallel:
    // the writes (10k small file creates + Windows Defender scans) dominate, so
    // overlapping their I/O waits is the win — decompression itself is trivial.
    use rayon::prelude::*;
    let count = rd_u32(&raw, &mut o)?;
    let mut jobs: Vec<(std::path::PathBuf, &[u8])> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let pl = rd_u16(&raw, &mut o)? as usize;
        let pb = raw.get(o..o + pl).ok_or("base bundle: eof path")?;
        o += pl;
        let rel = std::str::from_utf8(pb).map_err(|_| "base bundle: bad path utf8")?;
        let len = rd_u32(&raw, &mut o)? as usize;
        let data = raw.get(o..o + len).ok_or("base bundle: eof data")?;
        o += len;
        jobs.push((cache_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR)), data));
    }
    jobs.par_iter().try_for_each(|(dst, data)| {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(dst, data).map_err(|e| format!("{}: {e}", dst.display()))
    })
}
