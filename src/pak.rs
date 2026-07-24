//! Reader for the TL2 PAK / `.MOD` container — the inverse of this crate's
//! writer (`packer::w_header` / `w_manifest` + the data-section block layout).
//!
//! Two on-disk shapes share one format:
//!   * **`.MOD`**   — mod header, then the data section, then the manifest, all
//!                    in one file (offsets in the header locate the sections).
//!   * **`DATA.PAK` + `DATA.PAK.MAN`** — the base game archive: the data section
//!                    is the `.PAK`, the manifest is the sibling `.PAK.MAN`, and
//!                    there is no mod header (data starts at file offset 0).
//!
//! Data block  = `[u32 uncompressed_size][u32 compressed_size][ payload ]`,
//! where a compressed_size of 0 means the payload is stored raw; otherwise it
//! is a zlib stream. Manifest = `[u16 tag=2][u32 mhash][ss root][u32 fileCount]
//! [u32 dirCount]{ ss dirName, u32 recCount, { u32 crc, u8 type, ss name,
//! u32 off, u32 size, u64 ftime } }` — records with `type == TYPE_DIR` are
//! subdirectory markers, not files.
//!
//! Powers the interactive packer's targeted base extraction from `DATA.PAK`,
//! and is the basis for a future unpack-a-`.MOD` feature.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::packer::TYPE_DIR;

/// One file record from a manifest (subdirectory markers are dropped).
#[derive(Clone, Debug)]
pub struct Entry {
    /// Full manifest path, e.g. `MEDIA/UNITS/MONSTERS/FOO.DAT`.
    pub path: String,
    /// Manifest type code (see `packer::ext_type`).
    pub typ: u8,
    /// Offset of the data block within the data section.
    pub off: u32,
    /// Uncompressed size.
    pub size: u32,
}

/// A parsed manifest: the archive root (`MEDIA/`) plus every file record.
pub struct Manifest {
    pub root: String,
    pub files: Vec<Entry>,
}

fn rd_u16(b: &[u8], o: &mut usize) -> Result<u16, String> {
    let s = b.get(*o..*o + 2).ok_or("manifest: eof (u16)")?;
    *o += 2;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}
fn rd_u32(b: &[u8], o: &mut usize) -> Result<u32, String> {
    let s = b.get(*o..*o + 4).ok_or("manifest: eof (u32)")?;
    *o += 4;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn skip(b: &[u8], o: &mut usize, n: usize) -> Result<(), String> {
    if *o + n > b.len() {
        return Err("manifest: eof (skip)".into());
    }
    *o += n;
    Ok(())
}
/// Short string: `u16` UTF-16LE code-unit count, then that many LE units.
fn rd_ss(b: &[u8], o: &mut usize) -> Result<String, String> {
    let n = rd_u16(b, o)? as usize;
    let bytes = b.get(*o..*o + 2 * n).ok_or("manifest: eof (string)")?;
    *o += 2 * n;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

/// Parse a manifest blob (a `.PAK.MAN`, or the manifest slice of a `.MOD`).
pub fn parse_manifest(b: &[u8]) -> Result<Manifest, String> {
    let mut o = 0usize;
    let _tag = rd_u16(b, &mut o)?; // 2
    let _mhash = rd_u32(b, &mut o)?;
    let root = rd_ss(b, &mut o)?;
    let _file_count = rd_u32(b, &mut o)?;
    let dir_count = rd_u32(b, &mut o)?;

    let mut files = Vec::new();
    for _ in 0..dir_count {
        let dname = rd_ss(b, &mut o)?;
        let nrec = rd_u32(b, &mut o)?;
        for _ in 0..nrec {
            let _crc = rd_u32(b, &mut o)?;
            let typ = *b.get(o).ok_or("manifest: eof (type)")?;
            o += 1;
            let name = rd_ss(b, &mut o)?;
            let off = rd_u32(b, &mut o)?;
            let size = rd_u32(b, &mut o)?;
            skip(b, &mut o, 8)?; // ftime u64
            if typ == TYPE_DIR {
                continue; // subdirectory marker, not a file
            }
            files.push(Entry {
                path: format!("{dname}{name}"),
                typ,
                off,
                size,
            });
        }
    }
    Ok(Manifest { root, files })
}

/// An open archive: the data file plus its parsed manifest.
pub struct PakReader {
    file: std::fs::File,
    /// Absolute file offset where the data section starts (0 for a `.PAK`,
    /// `off_data` for a `.MOD`).
    data_base: u64,
    manifest: Manifest,
    decomp: libdeflater::Decompressor,
}

impl PakReader {
    /// Open a base `DATA.PAK` (+ sibling `DATA.PAK.MAN`).
    pub fn open_pak(pak_path: &Path) -> Result<Self, String> {
        let mut man_os = pak_path.as_os_str().to_owned();
        man_os.push(".MAN");
        let man_path = PathBuf::from(man_os);
        let man_bytes = std::fs::read(&man_path)
            .map_err(|e| format!("{}: {e}", man_path.display()))?;
        let manifest = parse_manifest(&man_bytes)?;
        let file = std::fs::File::open(pak_path)
            .map_err(|e| format!("{}: {e}", pak_path.display()))?;
        Ok(Self {
            file,
            data_base: 0,
            manifest,
            decomp: libdeflater::Decompressor::new(),
        })
    }

    /// Open a `.MOD` (mod header locates the embedded manifest + data section).
    pub fn open_mod(mod_path: &Path) -> Result<Self, String> {
        let mut file = std::fs::File::open(mod_path)
            .map_err(|e| format!("{}: {e}", mod_path.display()))?;
        // Header prefix: ver(2) modver(2) gamever(8) off_data(4) off_man(4).
        let mut head = [0u8; 20];
        file.read_exact(&mut head)
            .map_err(|e| format!("{}: short header: {e}", mod_path.display()))?;
        let off_data = u32::from_le_bytes([head[12], head[13], head[14], head[15]]) as u64;
        let off_man = u32::from_le_bytes([head[16], head[17], head[18], head[19]]) as u64;
        file.seek(SeekFrom::Start(off_man)).map_err(|e| e.to_string())?;
        let mut man_bytes = Vec::new();
        file.read_to_end(&mut man_bytes).map_err(|e| e.to_string())?;
        let manifest = parse_manifest(&man_bytes)?;
        Ok(Self {
            file,
            data_base: off_data,
            manifest,
            decomp: libdeflater::Decompressor::new(),
        })
    }

    pub fn root(&self) -> &str {
        &self.manifest.root
    }

    pub fn entries(&self) -> &[Entry] {
        &self.manifest.files
    }

    /// Read + inflate one entry's bytes.
    pub fn read_entry(&mut self, e: &Entry) -> Result<Vec<u8>, String> {
        self.file
            .seek(SeekFrom::Start(self.data_base + e.off as u64))
            .map_err(|err| format!("seek {}: {err}", e.path))?;
        let mut hdr = [0u8; 8];
        self.file
            .read_exact(&mut hdr)
            .map_err(|err| format!("read block hdr {}: {err}", e.path))?;
        let usize_ = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let csize = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
        if csize == 0 {
            let mut out = vec![0u8; usize_];
            self.file
                .read_exact(&mut out)
                .map_err(|err| format!("read stored {}: {err}", e.path))?;
            Ok(out)
        } else {
            let mut comp = vec![0u8; csize];
            self.file
                .read_exact(&mut comp)
                .map_err(|err| format!("read block {}: {err}", e.path))?;
            let mut out = vec![0u8; usize_];
            let n = self
                .decomp
                .zlib_decompress(&comp, &mut out)
                .map_err(|err| format!("inflate {}: {err:?}", e.path))?;
            out.truncate(n);
            Ok(out)
        }
    }

    /// Extract every file whose manifest path satisfies `want` into `out_root`,
    /// laying it out under the manifest root (so `MEDIA/UNITS/X.DAT` lands at
    /// `out_root/UNITS/X.DAT`). Returns the number of files written.
    pub fn extract_where<F>(&mut self, out_root: &Path, want: F) -> Result<usize, String>
    where
        F: Fn(&str) -> bool,
    {
        let root = self.manifest.root.clone();
        let jobs: Vec<Entry> = self
            .manifest
            .files
            .iter()
            .filter(|e| want(&e.path))
            .cloned()
            .collect();
        let mut n = 0;
        for e in &jobs {
            let rel = e.path.strip_prefix(&root).unwrap_or(&e.path);
            let dst = out_root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
            }
            let data = self.read_entry(e)?;
            std::fs::write(&dst, &data).map_err(|err| format!("{}: {err}", dst.display()))?;
            n += 1;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only where a real base install is present; verifies the reader
    /// against `DATA.PAK` end-to-end (parse manifest → inflate a known block →
    /// the bytes match the extracted loose-MEDIA copy the game ships).
    #[test]
    fn reads_real_data_pak_if_present() {
        let pak = Path::new(r"E:\Torchlight 2\PAKS\DATA.PAK");
        if !pak.is_file() {
            eprintln!("skip: no DATA.PAK on this machine");
            return;
        }
        let mut r = PakReader::open_pak(pak).expect("open");
        assert_eq!(r.root(), "MEDIA/");
        assert!(r.entries().len() > 10_000, "manifest has many files");

        // Pick a known small entry and inflate it; size must match the record.
        let e = r
            .entries()
            .iter()
            .find(|e| e.path.eq_ignore_ascii_case("MEDIA/BOSSMUSIC.LAYOUT"))
            .cloned()
            .expect("BOSSMUSIC.LAYOUT present");
        let data = r.read_entry(&e).expect("inflate");
        assert_eq!(data.len(), e.size as usize, "inflated size == manifest size");

        // The PAK stores COMPILED content under the SOURCE name, so the entry
        // named ".LAYOUT" is really the BINLAYOUT bytes — it must equal the
        // extracted ".LAYOUT.BINLAYOUT" sibling, NOT the text ".LAYOUT" source.
        let compiled = Path::new(r"E:\Torchlight 2\MEDIA\BOSSMUSIC.LAYOUT.BINLAYOUT");
        if let Ok(disk) = std::fs::read(compiled) {
            assert_eq!(data, disk, "PAK entry == extracted .BINLAYOUT");
        }
    }
}
