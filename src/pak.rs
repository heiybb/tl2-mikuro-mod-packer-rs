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

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::packer::{Header, TYPE_DIR};

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
    /// Windows FILETIME of the source file (100 ns ticks since 1601), as the
    /// packer recorded it — an unpack restores it onto the written file.
    pub ftime: u64,
}

/// A parsed manifest: the archive root (`MEDIA/`) plus every file record.
pub struct Manifest {
    pub root: String,
    pub files: Vec<Entry>,
    /// The manifest's own validated hash, as stored (`packer::pak_manifest_hash`).
    pub mhash: u32,
    /// Record count as stored — files AND directory markers.
    pub file_count: u32,
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
fn rd_u64(b: &[u8], o: &mut usize) -> Result<u64, String> {
    let s = b.get(*o..*o + 8).ok_or("manifest: eof (u64)")?;
    *o += 8;
    Ok(u64::from_le_bytes(s.try_into().unwrap()))
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

/// Parse a `.MOD` header — the inverse of `packer::w_header`. Returns the same
/// `Header` shape `packer::build_header` synthesizes from MOD.DAT (so an unpack
/// can write MOD.DAT straight back out), plus the two section offsets. `mhash`,
/// `root` and `fc` stay zero here; they belong to the manifest.
pub fn parse_mod_header(b: &[u8]) -> Result<(Header, u32, u32), String> {
    let mut o = 0usize;
    let ver = rd_u16(b, &mut o)?;
    let modver = rd_u16(b, &mut o)?;
    let gamever = rd_u64(b, &mut o)?;
    let off_data = rd_u32(b, &mut o)?;
    let off_man = rd_u32(b, &mut o)?;
    let title = rd_ss(b, &mut o)?;
    let author = rd_ss(b, &mut o)?;
    let descr = rd_ss(b, &mut o)?;
    let website = rd_ss(b, &mut o)?;
    let download = rd_ss(b, &mut o)?;
    let modid = rd_u64(b, &mut o)?;
    let flags = rd_u32(b, &mut o)?;
    let req_hash = rd_u64(b, &mut o)?;
    let nreqs = rd_u16(b, &mut o)? as usize;
    let mut reqs = Vec::with_capacity(nreqs);
    for _ in 0..nreqs {
        let name = rd_ss(b, &mut o)?;
        let id = rd_u64(b, &mut o)?;
        let v = rd_u16(b, &mut o)?;
        reqs.push((name, id, v));
    }
    let ndels = rd_u16(b, &mut o)? as usize;
    let mut dels = Vec::with_capacity(ndels);
    for _ in 0..ndels {
        dels.push(rd_ss(b, &mut o)?);
    }
    if o as u32 != off_data {
        return Err(format!(
            "mod header: parsed {o} bytes but the data section starts at {off_data}"
        ));
    }
    Ok((
        Header {
            ver,
            modver,
            gamever,
            title,
            author,
            descr,
            website,
            download,
            modid,
            flags,
            req_hash,
            reqs,
            dels,
            mhash: 0,
            root: String::new(),
            fc: 0,
        },
        off_data,
        off_man,
    ))
}

/// Parse a manifest blob (a `.PAK.MAN`, or the manifest slice of a `.MOD`).
pub fn parse_manifest(b: &[u8]) -> Result<Manifest, String> {
    let mut o = 0usize;
    let _tag = rd_u16(b, &mut o)?; // 2
    let mhash = rd_u32(b, &mut o)?;
    let root = rd_ss(b, &mut o)?;
    let file_count = rd_u32(b, &mut o)?;
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
            let ftime = rd_u64(b, &mut o)?;
            if typ == TYPE_DIR {
                continue; // subdirectory marker, not a file
            }
            // A record with an EMPTY name addresses the directory itself, not a
            // file in it — the packer emits one (as a `Item::Placeholder`, off
            // and size 0) whenever it could not resolve content for an entry.
            // One shipped mod carries 22 of them, typed 18 rather than 7; GUTS'
            // own unpacker writes nothing for them, and so must we. Reading them
            // as files means seeking to data offset 0 — the section header, not
            // a block — and inventing a 600 MB length out of those bytes.
            if name.is_empty() {
                continue;
            }
            files.push(Entry {
                path: format!("{dname}{name}"),
                typ,
                off,
                size,
                ftime,
            });
        }
    }
    Ok(Manifest { root, files, mhash, file_count })
}

/// An open archive: the data file plus its parsed manifest.
/// Reads and inflates data blocks. Split out from `PakReader` so an unpack can
/// hand each worker its OWN handle + inflater: a block read is a seek plus a
/// read, which cannot be shared across threads, but the container is immutable
/// so any number of independent readers over it is safe.
pub struct BlockReader {
    file: std::fs::File,
    /// Absolute file offset where the data section starts (0 for a `.PAK`,
    /// `off_data` for a `.MOD`).
    data_base: u64,
    /// Absolute file offset where the data section ENDS (`off_man` for a `.MOD`,
    /// EOF for a `.PAK`) — every block length is validated against it.
    data_end: u64,
    decomp: libdeflater::Decompressor,
}

impl BlockReader {
    pub fn open(path: &Path, data_base: u64, data_end: u64) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self { file, data_base, data_end, decomp: libdeflater::Decompressor::new() })
    }
}

pub struct PakReader {
    /// The container path, so `block_reader()` can open independent handles.
    src: PathBuf,
    blocks: BlockReader,
    manifest: Manifest,
    /// The mod header — `Some` only for a `.MOD` (a `.PAK` has none).
    header: Option<Header>,
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
        // A .PAK is data all the way to EOF (its manifest is the sibling .MAN).
        let data_end = std::fs::metadata(pak_path).map(|m| m.len()).unwrap_or(u64::MAX);
        Ok(Self {
            src: pak_path.to_path_buf(),
            blocks: BlockReader::open(pak_path, 0, data_end)?,
            manifest,
            header: None,
        })
    }

    /// Open a `.MOD` (mod header locates the embedded manifest + data section).
    pub fn open_mod(mod_path: &Path) -> Result<Self, String> {
        let mut file = std::fs::File::open(mod_path)
            .map_err(|e| format!("{}: {e}", mod_path.display()))?;
        // Header prefix: ver(2) modver(2) gamever(8) off_data(4) off_man(4).
        // off_data is where the header ends, so it also sizes the header read.
        let mut head = [0u8; 20];
        file.read_exact(&mut head)
            .map_err(|e| format!("{}: short header: {e}", mod_path.display()))?;
        let off_data = u32::from_le_bytes([head[12], head[13], head[14], head[15]]) as u64;
        let off_man = u32::from_le_bytes([head[16], head[17], head[18], head[19]]) as u64;
        if off_data < 20 || off_man < off_data {
            return Err(format!(
                "{}: implausible section offsets (data {off_data}, manifest {off_man})",
                mod_path.display()
            ));
        }
        file.rewind().map_err(|e| e.to_string())?;
        let mut head_bytes = vec![0u8; off_data as usize];
        file.read_exact(&mut head_bytes)
            .map_err(|e| format!("{}: truncated header: {e}", mod_path.display()))?;
        let (header, _, _) = parse_mod_header(&head_bytes)?;

        file.seek(SeekFrom::Start(off_man)).map_err(|e| e.to_string())?;
        let mut man_bytes = Vec::new();
        file.read_to_end(&mut man_bytes).map_err(|e| e.to_string())?;
        let mut manifest = parse_manifest(&man_bytes)?;
        // Carry the manifest-side header fields back onto the header, so it is
        // the same shape the writer consumed.
        let mut header = header;
        header.root = manifest.root.clone();
        header.mhash = manifest.mhash;
        header.fc = manifest.file_count;
        manifest.root = header.root.clone();
        drop(file);
        Ok(Self {
            src: mod_path.to_path_buf(),
            blocks: BlockReader::open(mod_path, off_data, off_man)?,
            manifest,
            header: Some(header),
        })
    }

    pub fn root(&self) -> &str {
        &self.manifest.root
    }

    /// A fresh, independent reader over the same data section — one per worker
    /// thread when extracting in parallel.
    pub fn block_reader(&self) -> Result<BlockReader, String> {
        BlockReader::open(&self.src, self.blocks.data_base, self.blocks.data_end)
    }

    /// The parsed `.MOD` header (`None` for a `DATA.PAK`, which has none).
    pub fn header(&self) -> Option<&Header> {
        self.header.as_ref()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.manifest.files
    }

    /// Read + inflate one entry's bytes.
    pub fn read_entry(&mut self, e: &Entry) -> Result<Vec<u8>, String> {
        self.blocks.read_entry(e)
    }
}

impl BlockReader {
    /// Read + inflate one entry's bytes.
    pub fn read_entry(&mut self, e: &Entry) -> Result<Vec<u8>, String> {
        // A placeholder record (the packer's `Item::Placeholder`) carries no
        // data block at all — offset 0 is the data section's own
        // [maxCsz][rollingHash] header, so reading one would decode those bytes
        // as a block length.
        if e.off == 0 && e.size == 0 {
            return Ok(Vec::new());
        }
        let block_start = self.data_base + e.off as u64;
        if block_start + 8 > self.data_end {
            return Err(format!(
                "{}: block offset {} is past the data section",
                e.path, e.off
            ));
        }
        self.file
            .seek(SeekFrom::Start(block_start))
            .map_err(|err| format!("seek {}: {err}", e.path))?;
        let mut hdr = [0u8; 8];
        self.file
            .read_exact(&mut hdr)
            .map_err(|err| format!("read block hdr {}: {err}", e.path))?;
        let usize_ = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let csize = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
        // The lengths come from the file, so check them against the section
        // before allocating: a corrupt or mis-typed record would otherwise
        // reserve whatever those four bytes happen to say.
        let payload = if csize == 0 { usize_ } else { csize };
        if block_start + 8 + payload as u64 > self.data_end {
            return Err(format!(
                "{}: block claims {payload} bytes at offset {}, past the data section end",
                e.path, e.off
            ));
        }
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
}

impl PakReader {
    /// Provision the base data a full mod pack needs into `cache_dir` (used as
    /// `TL2_MEDIA_DIR`): the compiled base DATs the pipeline reads as TEXT —
    /// `LEVELSETS/*.DAT` (MPP piece tables + ROOMPIECES.RAW) and `UNITS/*.DAT`
    /// (UNITDATA.RAW BASEFILE inheritance) — decompiled back to source, plus the
    /// collision meshes the levelset pieces reference, extracted raw. Returns
    /// `(dats_written, meshes_written)`.
    pub fn extract_base(&mut self, cache_dir: &Path) -> Result<(usize, usize), String> {
        let root = self.manifest.root.clone();
        // Case-insensitive MEDIA-relative path -> entry, for mesh resolution.
        let by_rel: HashMap<String, Entry> = self
            .manifest
            .files
            .iter()
            .map(|e| {
                let rel = e.path.strip_prefix(&root).unwrap_or(&e.path).to_uppercase();
                (rel, e.clone())
            })
            .collect();

        // Base DATs the RAW/MPP builders resolve against, as TEXT.
        let dats: Vec<Entry> = self
            .manifest
            .files
            .iter()
            .filter(|e| {
                let p = e.path.to_uppercase();
                p.ends_with(".DAT") && (p.contains("LEVELSETS/") || p.contains("UNITS/"))
            })
            .cloned()
            .collect();

        // Read (inflate) the DAT blocks serially — one shared file handle — then
        // decompile in parallel (the writes below dominate; on Windows, 10k small
        // file creates + Defender scans are the real cost, so they run parallel).
        let mut dat_blocks: Vec<(String, bool, Vec<u8>)> = Vec::with_capacity(dats.len());
        for e in &dats {
            let bindat = self.read_entry(e)?;
            let rel = e.path.strip_prefix(&root).unwrap_or(&e.path).to_string();
            let is_ls = e.path.to_uppercase().contains("LEVELSETS/");
            dat_blocks.push((rel, is_ls, bindat));
        }
        let decompiled: Vec<(String, Vec<u8>, Vec<String>)> = dat_blocks
            .par_iter()
            .map(|(rel, is_ls, bindat)| {
                let text = crate::bindat_decompile::decompile_bindat(bindat)?;
                let dat_bytes = crate::bindat_decompile::to_dat_bytes(&text);
                let mut meshes = Vec::new();
                if *is_ls {
                    let mut pieces = HashMap::new();
                    crate::mpp::dat::parse_dat(&dat_bytes, &mut pieces);
                    for pd in pieces.values() {
                        for cf in &pd.collision_files {
                            let up = cf.replace('\\', "/").to_uppercase();
                            let m = up.strip_prefix("MEDIA/").unwrap_or(&up).to_string();
                            if !m.is_empty() {
                                meshes.push(m);
                            }
                        }
                    }
                }
                Ok::<_, String>((rel.clone(), dat_bytes, meshes))
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Read the referenced collision meshes serially (shared handle).
        let wanted: BTreeSet<String> =
            decompiled.iter().flat_map(|(_, _, m)| m.iter().cloned()).collect();
        let mut mesh_files: Vec<(String, Vec<u8>)> = Vec::with_capacity(wanted.len());
        for rel in &wanted {
            if let Some(e) = by_rel.get(rel) {
                let data = self.read_entry(e)?;
                let e_rel = e.path.strip_prefix(&root).unwrap_or(&e.path).to_string();
                mesh_files.push((e_rel, data));
            }
        }

        // One parallel write pass for every file (the I/O-bound bottleneck).
        let n_dat = decompiled.len();
        let n_mesh = mesh_files.len();
        decompiled
            .par_iter()
            .map(|(rel, bytes, _)| (rel, bytes))
            .chain(mesh_files.par_iter().map(|(rel, bytes)| (rel, bytes)))
            .try_for_each(|(rel, bytes)| write_cache(cache_dir, rel, bytes))?;
        Ok((n_dat, n_mesh))
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

/// Write `data` to `cache_dir/<media-relative path>`, creating parents.
fn write_cache(cache_dir: &Path, rel: &str, data: &[u8]) -> Result<(), String> {
    let dst = cache_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&dst, data).map_err(|e| format!("{}: {e}", dst.display()))
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

    /// A manifest can carry PLACEHOLDER records — empty name, type 18, offset
    /// and size 0 — for entries the packer could not resolve content for. They
    /// address the directory itself, so they must never surface as files:
    /// reading one seeks to data offset 0 (the section's own header) and decodes
    /// those bytes as a block length. One shipped mod carries 22 of them and
    /// used to abort the whole unpack with a 600 MB read.
    #[test]
    fn placeholder_records_are_not_files() {
        // dirCount=1, one dir "MEDIA/" holding: a real file, a dir marker, and
        // an empty-named placeholder.
        let mut m: Vec<u8> = Vec::new();
        let ss = |s: &str, out: &mut Vec<u8>| {
            let u: Vec<u16> = s.encode_utf16().collect();
            out.extend_from_slice(&(u.len() as u16).to_le_bytes());
            for c in u {
                out.extend_from_slice(&c.to_le_bytes());
            }
        };
        m.extend_from_slice(&2u16.to_le_bytes()); // tag
        m.extend_from_slice(&0u32.to_le_bytes()); // mhash
        ss("MEDIA/", &mut m);
        m.extend_from_slice(&3u32.to_le_bytes()); // fileCount
        m.extend_from_slice(&1u32.to_le_bytes()); // dirCount
        ss("MEDIA/", &mut m);
        m.extend_from_slice(&3u32.to_le_bytes()); // 3 records
        let mut rec = |typ: u8, name: &str, off: u32, size: u32, m: &mut Vec<u8>| {
            m.extend_from_slice(&0u32.to_le_bytes()); // crc
            m.push(typ);
            ss(name, m);
            m.extend_from_slice(&off.to_le_bytes());
            m.extend_from_slice(&size.to_le_bytes());
            m.extend_from_slice(&0u64.to_le_bytes()); // ftime
        };
        rec(0, "REAL.DAT", 8, 4, &mut m);
        rec(TYPE_DIR, "SUB/", 0, 0, &mut m);
        rec(18, "", 0, 0, &mut m); // the placeholder

        let man = parse_manifest(&m).expect("parse");
        let names: Vec<&str> = man.files.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names, vec!["MEDIA/REAL.DAT"], "only the real file is a file");
    }

    #[test]
    fn extract_base_from_real_pak_if_present() {
        let pak = Path::new(r"E:\Torchlight 2\PAKS\DATA.PAK");
        if !pak.is_file() {
            eprintln!("skip: no DATA.PAK");
            return;
        }
        let cache = std::env::temp_dir().join(format!("tl2base_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let mut r = PakReader::open_pak(pak).expect("open");
        let (dats, meshes) = r.extract_base(&cache).expect("extract");
        eprintln!("extracted {dats} base DATs + {meshes} collision meshes");
        assert!(dats > 0 && meshes > 0);

        // The decompiled DATs must be parseable text with pieces.
        let ls = cache.join("LEVELSETS");
        assert!(ls.is_dir(), "LEVELSETS written to cache");

        let mut total: u64 = 0;
        fn du(p: &Path, t: &mut u64) {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    let path = e.path();
                    if path.is_dir() {
                        du(&path, t);
                    } else {
                        *t += e.metadata().map(|m| m.len()).unwrap_or(0);
                    }
                }
            }
        }
        du(&cache, &mut total);
        eprintln!("base cache footprint: {:.1} MB", total as f64 / 1.0e6);

        std::fs::remove_dir_all(&cache).ok();
    }
}
