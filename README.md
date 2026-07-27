# tl2-mikuro-mod-packer-rs

*[中文](README.zh-CN.md) · [Русский](README.ru.md)*

A **`.MOD` packer and unpacker** for Torchlight 2 — a single executable, with no
need for the game's editor (GUTS).

- **Pack** — compile a mod source folder (`MOD.DAT` + `MEDIA/`) into a `.MOD` the game reads.
- **Unpack** — turn a `.MOD` (or the base game's `DATA.PAK`) back into an **editable text source tree**.

All three binary formats (BINDAT / BINLAYOUT / MPP) are compilers written from
scratch: parse text → serialise binary, never reading an existing binary, so an
edit to the text always compiles correctly. The two decompilers are the same in
reverse. Base game data is baked into the exe as ~3 MB of gzip, so **you can pack
without the game installed**.

The Python reference implementation `mikuro_mod_packer` is archived at GitHub
`heiybb/TL2-RE`; this repository is the only implementation in use.

> **Want the whole story?** How the container is assembled, what each of the five
> binary formats actually is, which single hash the game validates, how the pathing
> grid is baked — plus the real mechanism behind the classic failures (ticked in the
> launcher but nothing happens / a portrait turned into some other image / players
> can't move after a level transition):
> **[What's Actually Inside a .MOD](https://tl2-mod.chr.moe/en/devlog/tl2-mod-packing-full-analysis/)**
> on Emberworks. This README is the manual; that post is the explanation.

---

## Quick start

```powershell
cargo build --release
# produces target\release\tl2-mikuro-mod-packer.exe — one file, copy it anywhere
```

Three ways to drive it, depending on what you want:

| You want | Do this |
| --- | --- |
| Click through it | **Double-click the exe** (or drop it in a mod folder / the game folder and run it) → interactive UI |
| Scripting, CI | Run it with arguments → an ordinary command-line tool |
| One file at a time | The `compile-dat` / `decompile-layout` / … subcommands |

---

## A walk through the interface

Started with no arguments, it opens on the picker. The images below are the real
frames the program draws, in English — the [Chinese](README.zh-CN.md) and
[Russian](README.ru.md) READMEs show the same walkthrough in their own language.
Notice what is *not* translated: mod names, paths, and every CLI flag, because
those are what you type.

### 1 · Choose what to pack

![The PACK tab: two mod sources, one ticked](docs/img/tui-en-pack.png)

Every folder holding `MOD.DAT` shows up here — from the exe's own folder, its
`mods\` subfolder, and the folder itself if that is where you dropped the exe.
`↑↓` moves, `Space` ticks, `a` ticks everything, `Enter` starts. With nothing
ticked, `Enter` packs the row under the cursor.

Each row is tagged `level mod` or `content mod`. One test decides it — does
`MEDIA/LAYOUTS` hold any `*.LAYOUT`? — and that decides whether the pathing
walk-grids get baked, which is the slow part of a build.

### 2 · Watch it run

![The OUTPUT tab: a pack in progress, following the tail](docs/img/tui-en-output.png)

The view switches to OUTPUT and streams the packer's own log line by line, so a
long build never looks like it hung. `↑↓` scrolls (which stops the auto-follow),
`End` re-follows the tail, `c` clears the transcript, `q` quits — but not while a
run is going, so a child process is never orphaned.

### 3 · Unpack, the other direction

![The UNPACK tab: a local .MOD and a deployed one](docs/img/tui-en-unpack.png)

`.MOD` files found next to the exe, in `.\mods`, and — the one that matters — in
the game's own mods folder, each row tagged `local` or `game mods` so you can tell
your fresh build from what is installed. Output lands in `unpacked\<name>\` next
to the exe. **The game's mods folder is never written to by an unpack**, so taking
apart something you have installed is safe.

---

## Packing

### What counts as a mod source

A folder holding **`MOD.DAT`** and a **`MEDIA/`** tree. `MOD_FILE_NAME` in
`MOD.DAT` decides the output file name (the folder name is the fallback);
`NAME` / `DISPLAYNAME` decide the title the game shows.

### The command

```powershell
tl2-mikuro-mod-packer.exe [--in-place|--temp-copy] [--mpp {re,none}] [--raw {auto,none}] [--deploy] <mod_dir>
```

Four things happen, in order (`convert_all`):

1. `*.DAT` → BINDAT
2. `*.LAYOUT` → BINLAYOUT
3. `MEDIA/*.RAW` indexes are generated (AFFIXES / SKILLS / TRIGGERABLES / MISSILES / UI / UNITDATA / ROOMPIECES)
4. `.MPP` pathing walk-grids are baked (level mods only)

Then the container is written: header + data section + manifest, plus the two
hashes the game validates (rollingHash, manifest mhash).

### Where the compiled files land — pick one

This is the only question `--in-place` / `--temp-copy` answer:

| Flag | Behaviour |
| --- | --- |
| none (default) | Compile in memory — **nothing but the `.MOD` is written** |
| `--in-place` | **ALSO** leave the `.BINDAT` / `.BINLAYOUT` in your `MEDIA/` tree |
| `--temp-copy` | Compile on disk in a throwaway copy; your folder stays clean |

### Where the `.MOD` lands

| Flag | Output |
| --- | --- |
| none | `<mod_dir>\<MOD_FILE_NAME>_<timestamp>.MOD` |
| `--deploy` | `%USERPROFILE%\Documents\My Games\Runic Games\Torchlight 2\mods\<MOD_FILE_NAME>.MOD` |

`--deploy` resolves that path through the registry's Shell Folders, so a
OneDrive-redirected Documents folder is handled correctly.

### The remaining switches

- `--mpp re` (default) bakes the walk-grids; `--mpp none` keeps the `.mpp` files
  already in your source. A content mod (no `*.LAYOUT` under `MEDIA/LAYOUTS`) has
  nothing to bake, so `none` is simply faster there.
- `--raw auto` (default) generates RAW indexes on demand, per content type;
  `--raw none` skips them entirely.

### Are the baked walk-grids safe to ship?

Yes. Worth saying plainly, because the accuracy figures further down look alarming
out of context.

`--mpp re` bakes the grids offline instead of driving the editor, and lands at
~99.7% of cells byte-identical to a native bake. That number counts **cosmetic**
differences: a cell is `0` walkable, `1` wall, `255` void, and the pathfinder treats
1 and 255 alike — both impassable. Counting only what a player can feel (1 ≡ 255),
agreement is **99.89%**, and the worst tile measured is at the noise floor rather
than above it: our bake differs from a native one in 0.106% of its cells, while two
*native* bakes of that same tile differ from each other in 0.097%. The native baker
is not deterministic — its region box flips between 1100 and 1125 cells from run to
run — so "byte-exact" was never a target that existed, and the honest ruler is
native-vs-native. That is why the offline backend is the only one shipped.

### Base data (why packing needs it)

Two things a build cannot compute from the mod alone:

- `UNITDATA.RAW` follows each unit's `BASEFILE` chain into the **base game's**
  `UNITS` templates to read `UNITTYPE` / `LEVEL` / `RARITY` / `CREATEAS`.
- `.MPP` walk-grids are baked against the base game's **collision geometry**.

So the packer needs base data, and takes the first of three tiers it finds:

1. a loose `MEDIA/` next to the exe (a real extracted install)
2. `PAKS/DATA.PAK` — or `DATA.PAK` — beside it (extracted on first use)
3. **the bundle baked into the exe** (always there, ~3 MB gzip)

Tiers 2 and 3 unpack once into a `.tl2packer_base\` cache next to the exe and
reuse it. **All three produce byte-identical `.MOD` output** — verified on a level mod.

---

## Unpacking

### Why you cannot just decompress it

A `.MOD` is **not** an ordinary archive. The compilers ran before it was written,
and the container stores **compiled payloads under their source names**: `FOO.DAT`
actually holds BINDAT, `FOO.LAYOUT` actually holds BINLAYOUT. So unpacking has to
**decompile**, or the tree you get is full of files that look like sources and are
binaries.

```text
.MOD ─┬─ header      → MOD.DAT              (inverse of build_header)
      ├─ *.DAT       → bindat_decompile     → UTF-16LE text .DAT
      ├─ *.LAYOUT    → binlayout_decompile  → UTF-16LE text .LAYOUT
      └─ anything else                      → written verbatim
```

### The command

```powershell
tl2-mikuro-mod-packer.exe unpack <in.MOD|DATA.PAK> <out_dir> [--raw] [--no-times] [--filter <text>]
```

- `--raw` — do not decompile; write the payloads verbatim (`.DAT` stays BINDAT).
  Use it when you want to see what is really stored.
- `--no-times` — do not restore the file timestamps from the manifest.
- `--filter <text>` — only entries whose path contains the text (case-insensitive),
  e.g. `--filter SKILLS`.

What comes out **is a mod source folder**: edit it and pack it straight back. The
base game's `PAKS\DATA.PAK` (with its `DATA.PAK.MAN` sidecar) unpacks the same way;
it has no mod header, so no `MOD.DAT` is written.

### What an unpack tells you

It finishes by printing `note:` lines. None of them are errors, all are worth reading:

| Note | What it means / what to do |
| --- | --- |
| `gameVersion 0x… is not stored in MOD.DAT` | Packing reads it from the installed `Torchlight2.exe`; MOD.DAT has no such field. To reproduce the original, set `TL2_MOD_GAMEVER=0x…` before repacking |
| `UNK_<HEX8>` placeholder names | A DAT key name the dictionary could not reverse. It **compiles back to the same hash** (the game reads it fine), but the readable name is gone. ⚠️ Never hand-edit the digits in `UNK_` — those ARE the hash |
| `container lists N duplicate paths` | The container listed one path more than once (common in third-party mods). The last record wins — the same result serial extraction produced |
| `BINLAYOUT flag byte is 0x36, not the usual 0x04` | Packed by some other tool. A repack writes `0x04` |
| `will not decompile …; kept as FOO.LAYOUT.BINLAYOUT` | Decompilation failed; the payload is kept verbatim under the compiled name. A repack will not include that entry until a text source exists |

### Round-tripping (unpack → repack)

| Check | Result |
| --- | --- |
| Unit test `pack_unpack_repack_is_byte_identical` | pack → unpack → repack is **SHA256-identical** |
| A real third-party GUTS mod (熊猫联纵三号, 3452 files) | 3452/3453 byte-identical; the only difference is the **entry order** of the generated `UNITDATA.RAW` |
| 绿洲联盟 9 号 (73 files) | 73/74 identical, same cause (`UI.RAW` order) |

Three things the FIRST round cannot restore (inherent to the mechanism, not lost
data): **gameVersion** (see the table above), **key names outside the dictionary**
(emitted as `UNK_`), and the **manifest order of generated `MEDIA/*.RAW` files** —
they have no source file, so the packer appends them after the disk scan; once an
unpack puts them on disk, the next scan picks them up in sorted position. The entry
set, the contents and both validation hashes are unaffected, and **from the second
round on, unpack ↔ pack is completely byte-stable**.

Also: `packer::filetime_of` goes through `f64` seconds (a 53-bit mantissa cannot
hold 57-bit FILETIME ticks), so a restored timestamp can read back ±2 ticks (200 ns)
off. The field is informational only.

### Two safety boundaries that do not move

1. Manifest paths go through `sanitize_rel`: `..`, drive letters and backslashes are
   refused, so **a write outside the target directory is impossible**.
2. **A payload that will not decompile is never written under the source name.** It
   is parked as `FOO.LAYOUT.BINLAYOUT` with a warning — because `content_for` would
   otherwise "compile" whatever sits at the source name, and compiling a binary blob
   as layout text yields an **empty 8-byte layout**. That is what silently destroying
   data would look like.

---

## The interactive UI (no arguments)

Double-click the exe, or drop it into a mod folder / the game folder and run it, and
you get a ratatui interface with four tabs:

| Tab | Contents |
| --- | --- |
| **PACK** | The mod sources it found, each tagged content mod / level mod |
| **UNPACK** | The `.MOD` files it found, each tagged local / game mods |
| **OUTPUT** | The run log, streamed live; `c` clears it |
| **HELP** | Written from the **real code paths**: the three base tiers and which one is live, the actual scan/output paths, every CLI command |

Where it looks:

- PACK — the exe's own folder and its `mods\` subfolder (**that first folder counts
  itself** if it holds `MOD.DAT`)
- UNPACK — those two, **plus the game's mods folder** (where `--deploy` writes, so
  anything you just packed is right there)

Where it writes: PACK always deploys to the game's mods folder. UNPACK writes to
`unpacked\<name>\` next to the exe and **never touches the game's mods folder**, so
unpacking something you have installed is safe.

Keys: `↑↓` move · `Space` select · `a` all · `Enter` run · `Tab` / `1`-`4` switch tab ·
`?` help · `l` language · `c` clear the log (OUTPUT) · `q` or `Ctrl+C` quit.

The UI language follows the system language (English / Chinese / Russian);
`TL2_LANG=en|zh|ru` overrides it, and `l` cycles it live. To fix wording or add a
language without rebuilding, drop an edited ini next to the exe as `lang\<code>.ini`:
its keys win over the built-in ones, and anything you leave out falls back.

---

## Single-file tools and the verifier

```powershell
tl2-mikuro-mod-packer.exe compile-dat      <in.DAT>       <out.BINDAT>  [--global-dict <dict.bin>]
tl2-mikuro-mod-packer.exe compile-layout   <in.LAYOUT>    <out.BINLAYOUT>
tl2-mikuro-mod-packer.exe compile-mpp      <in.LAYOUT>    <out.mpp> [--media <MEDIA>] [--fallback <base MEDIA>]
tl2-mikuro-mod-packer.exe decompile-dat    <in.BINDAT>    <out.DAT>  [--utf8]
tl2-mikuro-mod-packer.exe decompile-layout <in.BINLAYOUT> <out.LAYOUT>
tl2-mikuro-mod-packer.exe extract-base     <DATA.PAK>     <out_dir>   # base LEVELSETS + collision meshes
tl2-mikuro-mod-packer.exe unpack-base      <out_dir>                  # the embedded base bundle; no install needed

verify.exe binlayout|binlayout-roundtrip|bindat|raw|mpp|mpp-corpus <media_dir> ...
verify.exe unpack <mod.MOD>     # unpack → repack → compare every entry's content
```

`compile-mpp --fallback <base MEDIA>` turns on the **mod-over-base VFS overlay** for a
single tile (look in `--media`'s mod first, then in `--fallback`'s base install), so
one tile resolves base-game link/mesh targets the same way a full build does. The full
build (the main command) already applies that overlay.

`decompile-dat` writes a **re-editable** UTF-16LE `.DAT` by default; `--utf8` gives a
plain-text dump for reading.

## Environment variables

| Variable | Effect |
| --- | --- |
| `TL2_MEDIA_DIR` | Base game MEDIA path (default `E:\Torchlight 2\MEDIA`) |
| `TL2_MOD_GAMEVER` | Override the header's gameVersion (used when round-tripping an unpack) |
| `TL2_LANG` | TUI language: `en` / `zh` / `ru` |
| `MIKURO_TIMING` | Print per-phase timings |
| `MIKURO_BINDAT_DICT` | Use corpus-mode BINDAT string ids (see the last section) |
| `TL2_MODPACK_LIST` | Skip the TUI and dump what it discovered to stdout (diagnostics / CI) |

---

## Verification status (2026-07-04, A/B against the Python reference)

| Module | How | Result |
| --- | --- | --- |
| BINLAYOUT | All 8985 base-game `.LAYOUT` files, bytes vs shipped | **8965/8985 byte-exact** (level with Python; the 20 misses are unreproducible uninitialised `@25` garbage in the shipped files) |
| BINDAT | 400 random samples + TAGS.DAT vs Python `compile_dat` | **401/401 identical** (including TAGS.DAT with its illegal UTF-16 surrogate) |
| RAW ×7 | vs the shipped base-game `MEDIA/*.RAW` | **7/7 byte-exact** (AFFIXES/SKILLS/TRIGGERABLES/MISSILES/UI/UNITDATA/ROOMPIECES) |
| MPP (offline `re`) | 13 varied tiles (cliff / landmark alias / boss / town / exit / cave / vault) vs Python `compile_mpp` | **13/13 identical** |
| Whole `.MOD` | Three real mods vs the Python build | header + manifest structurally identical; **all 1856 / 35864 / 11205 files match on decompressed content, CRC and ftime**; rollingHash + mhash self-consistent (the two hashes the game checks) |
| BINLAYOUT decompile fixed point | our BINLAYOUT → decompile → recompile | **8986/8986 byte-exact** |
| same, from shipped binaries | 8967/8986 — the 19 failures are **exactly** the 19 compilation itself cannot reproduce; the decompiler adds none |

The only byte difference from the Python output is the zlib stream (different
compression backend; the game only inflates, and the Rust output is slightly smaller).

`src/binlayout_decompile.rs` reaches a fixed point because BINLAYOUT carries its full
structure: the **same embedded schema** (`data/binlayout_schema.json`) names everything
in reverse — unlike BINDAT, which needs a dictionary. What the compiler discards is
rebuilt to the standard of "recompiles byte-identically": `PARENTID` is recovered from
the object tree; a Group's `CHOICE` / `GAME MODE` / `TAG` / `RANDOMIZATION` / `NUMBER` /
`NO TAG FOUND` / `LEVEL UNIQUE` / `ACTIVE|DEACTIVE THEMES` exist only in the trailing
datagroup and are paired back onto the Group objects structurally; defaults
(RANDOMIZATION 1, NUMBER 1, no TAG) are emitted only when they are not the default.
Default orientations (`FORWARD` Z==1 / `RIGHT` X==1 / `UP` Y==1, skipped at compile
time) are **deliberately not restored**: shipped `.LAYOUT` sources do not write them
either, and inventing one **asserts** an orientation to every downstream reader — a
reader deriving `UP` from `FORWARD × RIGHT` would take the fake value.

## Performance — the whole 挑战者大陆 series, 10 components (2026-07-04, 16 cores, warm cache, outputs verified file-by-file)

After optimisation (parallel Pass-A content reads + the libdeflate backend + concurrent, lock-free RAW):

| Component | Python (isal + numba + multiprocessing) | Rust | Speedup | .MOD size Rust / Python |
| --- | --- | --- | --- | --- |
| 宠物系统 | 1.27s | **0.32s** | 4.0× | 0.41 / 0.44 MB |
| 护身符 | 0.81s | **0.30s** | 2.7× | 0.73 / 0.76 MB |
| 暗黑世界(临时) | 5.07s | **0.64s** | 7.9× | 1.38 / 1.57 MB |
| 至尊适配 | 1.30s | **0.46s** | 2.8× | 25.8 / 26.7 MB |
| 实验内容 | 1.66s | **0.40s** | 4.2× | 18.0 / 18.5 MB |
| 佣兵系统 | 2.78s | **0.64s** | 4.3× | 19.7 / 20.6 MB |
| 暗黑传奇 | 8.98s | **3.93s** | 2.3× | 43.7 / 46.0 MB |
| 群魔堕落 | 10.32s | **5.89s** | 1.8× | 76.5 / 79.1 MB |
| 职业技能 | 13.53s | **5.98s** | 2.3× | 106.6 / 111.7 MB |
| 通用素材01 | 18.14s | **5.16s** | 3.5× | 263.5 / 277.8 MB |
| **Total** | **63.9s** | **23.7s** | **2.69×** | 556 / 603 MB |

Every pair passed the structural check: identical header/manifest, identical
decompressed content + CRC + ftime for every file, self-consistent rollingHash +
mhash. The only byte difference is the zlib stream (libdeflate; the Rust output
averages ~5% smaller). RAW 7/7 and BINLAYOUT 8965/8985 against the shipped corpus
were re-verified after the optimisation and are still byte-exact. (The Python side
was fully optimised too: isal SIMD compression, numba-JIT MPP kernels, process/thread
pools; from a cold start Python also pays numba compilation, widening the gap.)

`MIKURO_TIMING=1` prints the per-phase timings used for tuning. What is left is the
Windows per-file open cost (tens of thousands of small AFFIXES/SKILLS reads, already
spread across threads; Defender's real-time scanning is the main driver) and the MPP
classifier (~1.9s per 170 tiles). There is little more to win.

On the unpack side: going parallel took it from 1.02 to 0.23 ms/file — 87764 files in
19.7s instead of 82.4s (**4.2×**) — with output verified byte-identical to the serial
build (content + mtime, across 73848 files).

## Final comparison — vs the native GUTS editor DLL (5 repeats)

The native numbers come from the native benchmark in `D:\WizardProject\TL2\TL2-RE\README.md`
(a forked headless `tl2_console_fork` driving the real EditorGuts.dll; InitEditor's
one-off 3.85s is amortised, warm timing over `CreateMod` + `EditorRegenPathingData`).
These 5 Rust runs used the **same source copies as the native benchmark**
(`E:\Torchlight 2\mods`, which carries compiled siblings, hence the higher file counts).

| Component | Files | native build | native MPP | native total | Rust ×5 (s) | Rust mean | Speedup |
| --- | --: | --: | --: | --: | --- | --: | --: |
| 通用素材01 | 12712 | 164.05 | 208.77 | 372.82 | 6.46/11.75/12.37/6.25/6.05 | **8.58** | **43.5×** |
| 职业技能 | 68217 | 268.42 | 0.00 | 268.42 | 12.14/19.55/8.85/9.30/11.05 | **12.18** | **22.0×** |
| 群魔堕落 | 52438 | 169.36 | 0.51 | 169.87 | 12.79/7.56/7.66/8.09/16.58 | **10.54** | **16.1×** |
| 暗黑传奇 | 32020 | 97.86 | 6.02 | 103.88 | 9.46/5.21/5.36/9.76/10.18 | **7.99** | **13.0×** |
| 暗黑世界(临时) | 354 | 9.68 | 17.26 | 26.94 | 0.89/0.76/2.10/2.30/1.73 | **1.56** | **17.3×** |
| 佣兵系统 | 2956 | 14.16 | 0.86 | 15.02 | 1.09/0.79/1.02/2.48/2.47 | **1.57** | **9.6×** |
| 至尊适配 | 1818 | 11.14 | 0.00 | 11.14 | 0.82/0.56/0.57/1.23/1.31 | **0.90** | **12.4×** |
| 实验内容 | 470 | 8.31 | 0.00 | 8.31 | 0.65/0.47/0.47/1.07/1.06 | **0.74** | **11.2×** |
| 护身符 | 1348 | 4.29 | 0.00 | 4.29 | 0.48/0.36/0.96/0.99/0.33 | **0.62** | **6.9×** |
| 宠物系统 | 431 | 1.96 | 0.17 | 2.13 | 0.51/0.43/1.83/1.76/0.38 | **0.98** | **2.2×** |
| **Total** | | 749.2 | 233.6 | **982.8** | | **45.66** | **21.5×** |

- Summing each component's best of 5 gives 31.0s → **31.7×**; the run-to-run spread is
  mostly Defender's real-time scanning and write-back cache flushes (each round writes
  ~600 MB of `.MOD`).
- Same workload across three pipelines: native GUTS 983s / Python (fully optimised
  isal + numba) ~64s / **Rust 46s**.
- 地图拓展 (242.98s) and POE (52.72s) appear in the native table but their source
  copies were deleted and cannot be re-measured, so they are excluded from the total.
- MPP is not like-for-like: native = EditorRegenPathingData (byte-exact); Rust = the
  offline `re` backend (≈99.7% cell-accurate, a known ceiling — actual walkability is
  at the float floor, see below).

### Whole-corpus MPP A/B (`verify mpp-corpus`, 2026-07-04)

Against the 1293-tile DLL oracle corpus (`TL2-RE/tools/mpp_corpus`):

```
compile+compare 1293 tiles: 10.3s   (context 0.1s)
byte-exact files: 118/1293   dim-match: 1272/1293
cells: 95,773,940   diff: 276,839   accuracy: 99.7109%
```

**The diff count matches the Python/numba archive figure (276839 / 95.7M) cell for
cell** — an operation-by-operation port of the f64 kernels is cell-for-cell equivalent
at corpus scale. These **vanilla-corpus** numbers are unaffected by the mod fixes below
(vanilla has no Layout-Link-to-base and no clone chains; both fixes change zero bytes
across all 1293 tiles).

### Mod-tile A/B + the Layout-Link fixes (2026-07-12)

The vanilla shipped `.mpp` files were baked years ago, and the native baker is itself
nondeterministic (below), so **mod** tiles were compared against a **fresh native bake**
as the oracle (`TL2-Mikuro-Console --clean --twice`), cell by cell, over 17 ACT5 tiles
from 挑战者「通用素材01」. That found and fixed two **MPP+LAYOUT coupled** paths:

1. **Layout-Link resolution falls back to the base install** (`resolve_link_layout` gained
   `fallback_dir`, a mod-over-base VFS, matching how collision meshes resolve). A mod's
   link pointing at a base-game prop layout (e.g. `CABIN_01_NOSPAWNER.LAYOUT`, present in
   the install but not the mod) used to fail to resolve → its collision never entered the
   bake → the whole prop footprint under-blocked (a 2475-cell hole in DARKWOODS).
2. **Transform inheritance: a Layout-Link parent does NOT pass its transform down**
   (`compute_world` extends the existing Room-Piece clone bookkeeping to `Layout Link*`).
   DARKWOODS has Layout-Link-under-Layout-Link clone chains (local scale 2–3 per level,
   ×72); the native bake's box of 1100×1626 **requires** no inheritance (full inheritance
   blows it up to 1776×2050) — established from native output, not by trial and error.

Result across the 17 tiles vs a fresh native bake: **99.7232% → 99.7718%** aggregate,
12 improved and 0 regressed (DARKWOODS −3171, DARKWOODS111 −3167, NETHER −2426); the
vanilla corpus is byte-unchanged; `compile-mpp` output is **byte-exact** with the Python
reference on those 17 tiles plus 8 vanilla ones.

**Native nondeterminism is the real float ceiling (measured).** Bake the same mod tile
twice: DARKWOODS's region box flips between **1100 and 1125** (±25 cells = one 10-unit
snap tile; a collision bound sits exactly on the snap threshold and float noise pushes
some runs over it), and two bakes of the same box still differ by ~0.1–0.6% of cells
(cliff oscillation). **⇒ Byte-exactness against an oracle that is not itself
deterministic is impossible in principle; the right floor to measure against is
native-vs-native, not byte-exact-vs-one-bake.**

**Actual walkability is at the float floor.** Cell semantics: `0 = walkable`,
`1 = wall`, `0xFF = void`; to the pathfinder 1 and 255 are both impassable. Re-measured
"de-cosmetically" (1 ≡ 255): **99.8934%** aggregate, THECROSSING **99.8938% ≈ its own
float floor of 0.0965%**. About half of the raw-byte residual is 255↔1 void/wall
labelling inside tree clusters (no effect on walkability within a tile, only at tile
seams); the rest is irreducible float. **In other words the grid's real walkability
already matches native to the float floor — which is why the offline backend is the
only one kept.**

The vanilla-corpus residual above (276839 cells) is the same information-theoretic
floor (native cliff/multichunk nondeterminism + per-instance runtime nocollide); no
change of language or precision moves it.

## Mapping to the Python implementation

| Rust | Python | Notes |
| --- | --- | --- |
| `src/rghash.rs` | `rghash.py` | RGHash (== EditorGuts sub_100CA9A0), hashed per code point |
| `src/text.rs` | — | The UTF-16 surrogatepass equivalent layer (`WStr = Vec<u32>` code points) |
| `src/bindat.rs` | `bindat.py` | Per-file hash string ids (`HashStringDict`); sort and probe semantics aligned one by one |
| `src/binlayout.rs` | `binlayout.py` | The pure DLL schema, embedded with `include_str!` (`data/*.json`) |
| `src/raw.rs` | `raw.py` | Both scan orders (files-first / interleaved) + the UNITDATA BASEFILE inheritance chain |
| `src/packer.rs` | `packer.py` | Container writer, rollingHash/mhash, manifest tree, convert_all orchestration |
| `src/binlayout_decompile.rs` | — | BINLAYOUT → LAYOUT (schema reverse lookup; fixed point 8986/8986) |
| `src/bindat_decompile.rs` | — | BINDAT → DAT (embedded dictionary + the `UNK_<HEX8>` escape) |
| `src/pak.rs` / `src/unpack.rs` | — | Container reading (header/manifest/blocks) and `.MOD` → source tree |
| `src/tui.rs` | — | The ratatui interface (binary-only; never reaches wasm) |
| `src/mpp/*` | `mpp/*` | Region-header f32 rounding aligned per operation; the native classifier is `build_grid_fast`-equivalent (bucket culling, row-parallel with rayon) |

Bug-for-bug behaviour kept on purpose: ogre_mesh reads only the first vertex-declaration
element (Python's +14 stride bug; POSITION is always element 0); binlayout swallows the
first occurrence of `NAME` / `CHOICE` / `GAME MODE` as object metadata; TAGS.DAT's empty
root-name hash. See the comments in each file.

Not ported: `rules.py` (multichunk classification only affects diagnostics, never the
`.mpp` bytes).

For the browser build — the same code shared via `#[path]`, compiled to wasm — see
`../tl2-mikuro-mod-packer-wasm`.

## BINDAT's two id modes (since 2026-07-04)

After a function-by-function audit of EditorGuts, `sub_10289A40`'s **exact** behaviour
is implemented as well (previously only the per-file hash id mode existed):

- **hash mode (default)** — string id = per-file RGHash + linear probing. No corpus
  dependency, naturally parallel, and verified against how the game parses the in-file
  table. Production packing uses this.
- **corpus mode** — string id comes from a rebuilt dictionary of the GUTS global interner
  (`data/bindat_string_dict.bin`, 154627 strings, 0.05s to load once); unknown strings get
  `max_id+1, +2, …` in **first-appearance order**, matching `sub_1023E9F0`'s `id = counter++`.
  Output is **15976/16084 byte-exact** against the shipped base game (the same count as the
  Python reference; the 108 differences are known shipped float quirks + TAGS.DAT, not
  implementation problems). Enable with `MIKURO_BINDAT_DICT=<dict.bin>` when packing, or
  `compile-dat <in> <out> --global-dict <dict.bin>` for one file; verify with
  `verify bindat-corpus <media_dir> <dict.bin>`.

**Performance**: corpus mode is slightly *faster* (whole corpus, 16084 files: ~1.0s vs
~1.2–1.7s; 职业技能's 22027 DATs end-to-end 1.59s vs 1.75s) — hash mode pays a sorted
BTreeSet plus probing per file, while corpus mode is a read-only lookup with the
dictionary shared lock-free across rayon workers. **No performance regression.**

**Why hash is still the default**: the corpus dictionary carries 715 majority-vote id
collisions from the shipped corpus (two different strings sharing one id; when both occur
in one file the later entry overwrites the earlier). Hash mode guarantees ids are unique
within a file, which is the safer production default; corpus mode is for byte-level
diffing and auditing against GUTS output.
