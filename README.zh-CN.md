# tl2-mikuro-mod-packer-rs

*[English](README.md)*

Torchlight 2 的 **`.MOD` 打包器 + 解包器**,单文件 exe,不需要装游戏编辑器(GUTS)。

- **打包**:把一个 mod 源目录(`MOD.DAT` + `MEDIA/`)编译成游戏能读的 `.MOD`。
- **解包**:把 `.MOD`(或基础游戏的 `DATA.PAK`)还原成**可编辑的文本源目录**。

三种二进制格式(BINDAT / BINLAYOUT / MPP)都是从零写的编译器:解析文本 → 序列化二进制,
不读取任何已有二进制,所以文本改动总能正确编译。反向的两个反编译器同理。基础游戏数据
以 ~3MB gzip 内嵌在 exe 里,所以**没装游戏也能打包**。

Python 参考实现 `mikuro_mod_packer` 已归档于 GitHub `heiybb/TL2-RE`,本仓库是唯一在用的实现。

---

## 快速开始

```powershell
cargo build --release
# 产物:target\release\tl2-mikuro-mod-packer.exe(单文件,可直接拷走)
```

三种用法,按你想干的事选:

| 你想 | 怎么做 |
| --- | --- |
| 点几下就打包/解包 | **双击 exe**(或把它丢进 mod 目录/游戏目录再运行)→ 进交互界面 |
| 写进脚本、CI | 带参数跑 → 就是普通命令行工具 |
| 只处理单个文件 | `compile-dat` / `decompile-layout` 等单文件子命令 |

---

## 打包

### 什么算一个 mod 源目录

一个文件夹,里面有 **`MOD.DAT`** 和 **`MEDIA/`** 树。`MOD.DAT` 里的 `MOD_FILE_NAME`
决定输出文件名(没有就用文件夹名),`NAME`/`DISPLAYNAME` 决定游戏里显示的标题。

### 命令

```powershell
tl2-mikuro-mod-packer.exe [--in-place|--temp-copy] [--mpp {re,none}] [--raw {auto,none}] [--deploy] <mod目录>
```

打包时依次发生四件事(`convert_all`):

1. `*.DAT` → BINDAT
2. `*.LAYOUT` → BINLAYOUT
3. 生成 `MEDIA/*.RAW` 索引(AFFIXES / SKILLS / TRIGGERABLES / MISSILES / UI / UNITDATA / ROOMPIECES)
4. 烘 `.MPP` 寻路走格(只有 level mod 需要)

然后写容器:header + 数据段 + manifest,并算两个游戏会校验的哈希(rollingHash、manifest mhash)。

### 编译产物落在哪 —— 三选一

这是 `--in-place` / `--temp-copy` 唯一要回答的问题:

| 写法 | 行为 |
| --- | --- |
| 不加(默认) | 只在内存里编译,**除了 `.MOD` 什么都不写** |
| `--in-place` | **额外**把 `.BINDAT` / `.BINLAYOUT` 留在你的 `MEDIA/` 树里 |
| `--temp-copy` | 复制一份到临时目录里编译,你的目录保持干净 |

### `.MOD` 落在哪

| 写法 | 输出 |
| --- | --- |
| 不加 | `<mod目录>\<MOD_FILE_NAME>_<时间戳>.MOD` |
| `--deploy` | `%USERPROFILE%\Documents\My Games\Runic Games\Torchlight 2\mods\<MOD_FILE_NAME>.MOD` |

`--deploy` 的路径走注册表 Shell Folders 解析,OneDrive 重定向也安全。

### 其余开关

- `--mpp re`(默认)现烘走格;`--mpp none` 保留源目录里已有的 `.mpp`。
  content mod(`MEDIA/LAYOUTS` 下没有 `*.LAYOUT`)本来就没东西可烘,给 `none` 更快。
- `--raw auto`(默认)按内容类型**按需**生成 RAW 索引;`--raw none` 完全跳过。

### base 数据(为什么打包需要它)

有两件事光看 mod 自己算不出来:

- `UNITDATA.RAW` 要沿每个单位的 `BASEFILE` 继承链,进**基础游戏**的 `UNITS` 模板读
  `UNITTYPE` / `LEVEL` / `RARITY` / `CREATEAS`。
- `.MPP` 走格要拿基础游戏的**碰撞几何**做烘焙。

所以打包器需要 base 数据,取三层里第一个可用的:

1. exe 旁边的散装 `MEDIA/`(真实解包过的安装)
2. 旁边的 `PAKS/DATA.PAK` 或 `DATA.PAK`(首次使用时抽取)
3. **内嵌在 exe 里的 bundle**(永远可用,~3MB gzip)

第 2、3 层只在首次解到 exe 旁的 `.tl2packer_base\` 缓存,之后复用。
**三层产出的 `.MOD` 字节完全相同**(已在一个 level mod 上验证)。

---

## 解包

### 为什么不能直接解压

`.MOD` **不是**普通压缩包。打包时编译器先跑过一遍,容器里存的是**编译产物,却挂在源文件名下**:
`FOO.DAT` 里其实是 BINDAT,`FOO.LAYOUT` 里其实是 BINLAYOUT。所以解包必须**反向编译**,
否则你拿到的树里全是「看着像源码、其实是二进制」的文件。

```text
.MOD ─┬─ header      → MOD.DAT              (build_header 的逆)
      ├─ *.DAT       → bindat_decompile     → UTF-16LE 文本 .DAT
      ├─ *.LAYOUT    → binlayout_decompile  → UTF-16LE 文本 .LAYOUT
      └─ 其它                                → 原样写出
```

### 命令

```powershell
tl2-mikuro-mod-packer.exe unpack <in.MOD|DATA.PAK> <out目录> [--raw] [--no-times] [--filter <文本>]
```

- `--raw`:不反编译,把载荷原样写出(`.DAT` 保持 BINDAT)。想看真实存储内容时用。
- `--no-times`:不还原 manifest 里的文件时间戳。
- `--filter <文本>`:只解路径里含该文本的条目(大小写不敏感),例如 `--filter SKILLS`。

产出的目录**就是一个 mod 源目录**,可以直接改完再打包回去。
基础游戏的 `PAKS\DATA.PAK`(带 sidecar `DATA.PAK.MAN`)也能解,只是没有 mod header,不写 `MOD.DAT`。

### 解包会告诉你的事

解包结束会打印 `note:`,都不是错误,但值得看一眼:

| 提示 | 含义 / 怎么办 |
| --- | --- |
| `gameVersion 0x… is not stored in MOD.DAT` | 打包时从已安装的 `Torchlight2.exe` 读,MOD.DAT 里没这个字段。要复现原值就 `TL2_MOD_GAMEVER=0x…` 再打包 |
| `UNK_<HEX8>` 占位名 | 词表反查不到的 DAT key 名。它**编译回同一个 hash**(游戏照读),但可读名字没了。⚠️ 永远别手改 UNK_ 里的数字 —— 你改的是 hash |
| `container lists N duplicate paths` | 容器把同一路径列了多次(第三方 mod 常见)。保留最后一条 —— 与串行解包结果一致 |
| `BINLAYOUT flag byte is 0x36, not the usual 0x04` | 别的工具打的包。重打包会写 `0x04` |
| `will not decompile …; kept as FOO.LAYOUT.BINLAYOUT` | 反编译失败,载荷**原样留下并改名**;重打包不会包含这条,除非你补上文本源 |

### 往返(解包 → 重打包)

| 检查 | 结果 |
| --- | --- |
| 单元测试 `pack_unpack_repack_is_byte_identical` | 打包→解包→重打包 **SHA256 完全相同** |
| 真实第三方 GUTS mod(熊猫联纵三号,3452 文件) | 3452/3453 逐字节相同,唯一差异 = 生成型 `UNITDATA.RAW` 的**条目顺序** |
| 绿洲联盟 9 号(73 文件) | 73/74 相同,唯一差异同为生成型 `UI.RAW` 顺序 |

第一轮往返**不能**还原三件事(机制固有,不是丢数据):**gameVersion**(见上表)、
**词表外的 key 名**(发 `UNK_`)、**生成型 `MEDIA/*.RAW` 在 manifest 里的顺序**(它们原本没有
源文件,打包器把它们追加在磁盘扫描之后;解包落盘后下次扫描就按排序位置收编)。条目集合、
内容、两个校验哈希都不受影响,**从第二轮起解包↔打包完全字节稳定**。

另注:`packer::filetime_of` 走 `f64` 秒(53 位尾数装不下 57 位 FILETIME ticks),
还原的时间戳重读可能差 ±2 tick(200ns)。该字段只是信息性的。

### 两个不动的安全边界

1. manifest 路径经 `sanitize_rel` 过滤:`..`、盘符、反斜杠一律拒绝,**不可能写出目标目录**。
2. **反编译失败的载荷不会挂到源文件名上**,而是落成 `FOO.LAYOUT.BINLAYOUT` 并报警 ——
   因为 `content_for` 会去「编译」源文件名位置的东西,拿二进制当 layout 文本编译的结果是
   一个**空的 8 字节 layout**,那才是真的静默毁数据。

---

## 交互界面(无参数启动)

双击 exe,或把它丢进 mod 目录 / 游戏目录再运行,会进 ratatui 界面。四个页签:

| 页签 | 内容 |
| --- | --- |
| **PACK** | 扫到的 mod 源目录。每行标 content mod / level mod |
| **UNPACK** | 扫到的 `.MOD` 文件。每行标 local / game mods |
| **OUTPUT** | 运行日志(实时流式);`c` 清空 |
| **HELP** | 从**真实代码路径**生成的说明:base 三层、当前用的是哪层、扫描/输出的真实路径、CLI 全部命令 |

扫描位置:

- PACK:exe 所在目录、它的 `mods\` 子目录(**这个目录自身**若含 `MOD.DAT` 也算一个源)
- UNPACK:上面两处,**加上游戏的 mods 目录**(`--deploy` 的落点,所以刚打的包马上就在列表里)

输出位置:PACK 一律 deploy 到游戏 mods 目录;UNPACK 写到 exe 旁的 `unpacked\<名字>\`,
**永远不写游戏 mods 目录**,所以解包一个你已装的 mod 是安全的。

按键:`↑↓` 移动 · `Space` 勾选 · `a` 全选 · `Enter` 执行 · `Tab`/`1`-`4` 切页 ·
`?` 帮助 · `l` 切语言 · `c` 清空日志(OUTPUT 页)· `q` 或 `Ctrl+C` 退出。

界面语言按系统语言自动选(英/中/俄),`TL2_LANG=en|zh|ru` 可覆盖,界面里 `l` 随时切。
想改措辞或加语言,不用重新编译:把改好的 ini 放到 exe 旁的 `lang\<code>.ini`,
里面的键覆盖内置的,没写的自动回落。

---

## 单文件工具与验证器

```powershell
tl2-mikuro-mod-packer.exe compile-dat      <in.DAT>       <out.BINDAT>  [--global-dict <dict.bin>]
tl2-mikuro-mod-packer.exe compile-layout   <in.LAYOUT>    <out.BINLAYOUT>
tl2-mikuro-mod-packer.exe compile-mpp      <in.LAYOUT>    <out.mpp> [--media <MEDIA>] [--fallback <base MEDIA>]
tl2-mikuro-mod-packer.exe decompile-dat    <in.BINDAT>    <out.DAT>  [--utf8]
tl2-mikuro-mod-packer.exe decompile-layout <in.BINLAYOUT> <out.LAYOUT>
tl2-mikuro-mod-packer.exe extract-base     <DATA.PAK>     <out目录>    # 抽 base LEVELSETS + 碰撞 mesh
tl2-mikuro-mod-packer.exe unpack-base      <out目录>                   # 解内嵌 base bundle(不需要装游戏)

verify.exe binlayout|binlayout-roundtrip|bindat|raw|mpp|mpp-corpus <media_dir> ...
verify.exe unpack <mod.MOD>     # 解包→重打包→逐条目内容对账
```

`compile-mpp --fallback <base MEDIA>`:单瓦片编译时启用 **mod-over-base VFS overlay**
(先查 `--media` 的 mod,再查 `--fallback` 的基础安装),用于把单个 mod 瓦片按整包构建的
口径解析基础游戏 link/mesh 目标。整包构建(主命令)已自动带此 overlay。

`decompile-dat` 默认输出 UTF-16LE 的**可再编辑** `.DAT`;`--utf8` 出的是给人看的纯文本 dump。

## 环境变量

| 变量 | 作用 |
| --- | --- |
| `TL2_MEDIA_DIR` | 基础游戏 MEDIA 路径(默认 `E:\Torchlight 2\MEDIA`) |
| `TL2_MOD_GAMEVER` | 覆写 header 里的 gameVersion(解包往返时用) |
| `TL2_LANG` | TUI 界面语言 `en` / `zh` / `ru` |
| `MIKURO_TIMING` | 打印分阶段计时 |
| `MIKURO_BINDAT_DICT` | 走 corpus 模式的 BINDAT string id(见文末) |
| `TL2_MODPACK_LIST` | 不进 TUI,把扫描结果 dump 到 stdout(诊断/CI) |

---

## 验证状态(2026-07-04,与 Python 参考实现 A/B 对比)

| 模块 | 验证方式 | 结果 |
| --- | --- | --- |
| BINLAYOUT | 基础游戏全语料 8985 个 .LAYOUT 字节对比 shipped | **8965/8985 字节精确**(与 Python 完全同级;20 个 miss 是 shipped 文件里不可复现的未初始化 `@25` 垃圾) |
| BINDAT | 400 随机样本 + TAGS.DAT,vs Python `compile_dat` | **401/401 字节一致**(含非法 UTF-16 surrogate 的 TAGS.DAT) |
| RAW ×7 | vs 基础游戏 shipped `MEDIA/*.RAW` | **7/7 字节精确**(AFFIXES/SKILLS/TRIGGERABLES/MISSILES/UI/UNITDATA/ROOMPIECES) |
| MPP (offline `re`) | 13 个多样 tile(cliff/landmark 别名/boss/town/exit/cave/vault)vs Python `compile_mpp` | **13/13 字节一致** |
| .MOD 整包 | 佣兵系统 / 职业技能 / 通用素材01 三个真实 mod,vs Python 整包 | header+manifest 结构一致;**全部 1856 / 35864 / 11205 个文件解压内容、CRC、ftime 一致**;rollingHash + mhash 自洽(游戏校验的两个哈希) |
| BINLAYOUT 反编译不动点 | 我们编译的 BINLAYOUT → 反编译 → 重编译 | **8986/8986 字节精确** |
| 同上,输入换 shipped 二进制 | 8967/8986 —— 失败的 19 个**正是**编译本身就复现不了的那 19 个,反编译器零新增 |

唯一与 Python 产物的字节差异 = zlib 压缩流(压缩后端不同;游戏只做 inflate,无感知,
且 Rust 产物略小)。

`src/binlayout_decompile.rs` 能做到不动点,是因为 BINLAYOUT 自带完整结构,用**同一份内嵌
schema**(`data/binlayout_schema.json`)反查即可命名一切 —— 不像 BINDAT 要靠词表。编译器
丢弃的信息按「重编译后逐字节相同」的口径重建:`PARENTID` 从对象树推回;Group 的
`CHOICE`/`GAME MODE`/`TAG`/`RANDOMIZATION`/`NUMBER`/`NO TAG FOUND`/`LEVEL UNIQUE`/
`ACTIVE|DEACTIVE THEMES` 只存在于尾部 datagroup,解析后按结构配对写回 Group 对象;默认值
(RANDOMIZATION 1、NUMBER 1、无 TAG)只在非默认时才发。**刻意不重建**默认朝向
(`FORWARD` Z==1 / `RIGHT` X==1 / `UP` Y==1 编译时被跳过):shipped `.LAYOUT` 源码同样不写
它们,凭空补一行等于向下游解析器**断言**了一个朝向,而按 `FORWARD × RIGHT` 推 `UP` 的读者
会被这行假数据带偏。

## 性能 — 挑战者大陆全系列 10 组件(2026-07-04,16 核,磁盘缓存预热后,产物内容逐文件校验一致)

优化后(并行 Pass A 内容读取 + libdeflate 压缩后端 + RAW 并发/去锁):

| 组件 | Python(isal+numba+多进程) | Rust | 加速 | .MOD 大小 Rust / Python |
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
| **全系列合计** | **63.9s** | **23.7s** | **2.69×** | 556 / 603 MB |

每一对 .MOD 均通过结构校验:header/manifest 一致、全部文件解压内容+CRC+ftime 一致、
rollingHash + mhash 自洽;唯一字节差异 = zlib 压缩流(libdeflate,Rust 产物平均还小
~5%)。RAW 7/7、BINLAYOUT 8965/8985 对 shipped 语料在优化后复验仍字节精确。
(Python 一侧已带满优化:isal SIMD 压缩、numba JIT 的 MPP 内核、进程/线程池;冷启动
时 Python 还要付 numba 编译等开销,差距更大。)

调优过程用 `MIKURO_TIMING=1` 输出分阶段计时。剩余热点是 Windows 每文件 open 开销
(AFFIXES/SKILLS 等万级小文件读取,已并行摊薄;Defender 实时扫描是主因)与 MPP
分类器(~1.9s/170 tile),继续压收益有限。

解包侧:并行提取后 1.02 → 0.23 ms/文件,87764 文件 82.4s → 19.7s(**4.2×**),
产出与串行版逐字节相同(内容 + mtime,73848 文件对账)。

## 最终对比 — vs 原生 GUTS 编辑器 DLL(挑战者大陆系列,5 次重复实验)

原生数据来自 `D:\WizardProject\TL2\TL2-RE\README.md` 的 native 基准(forked headless
`tl2_console_fork` 驱动真实 EditorGuts.dll,InitEditor 一次性 3.85s 已摊销,warm 计时
`CreateMod` + `EditorRegenPathingData`)。本次 Rust 5 轮实验跑在**与 native 基准相同的
源副本**上(`E:\Torchlight 2\mods`,含编译产物 sibling,故文件数大于 monorepo)。

| 组件 | 文件数 | 原生 build | 原生 MPP | 原生合计 | Rust 5 次 (s) | Rust 均值 | 加速 |
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
| **合计** | | 749.2 | 233.6 | **982.8** | | **45.66** | **21.5×** |

- 按各组件 5 次最优值合计 = 31.0s → **31.7×**;逐次波动主要来自 Defender 实时扫描与
  写回缓存冲刷(实验含每轮 ~600MB .MOD 写盘)。
- 三条管线同工作量参考:原生 GUTS 983s / Python(isal+numba 满优化)~64s / **Rust 46s**。
- 原生表中的 地图拓展(242.98s)与 POE(52.72s)源副本已删除,无法复测,未计入合计。
- MPP 口径差异:原生 = EditorRegenPathingData(字节精确);Rust = 离线 `re` 后端
  (≈99.7% cell 精确,已知天花板 —— 实际可走性已到浮点地板,见下)。

### MPP 全语料 A/B(`verify mpp-corpus`,2026-07-04)

对 1293-tile DLL oracle 语料(`TL2-RE/tools/mpp_corpus`)全量对比:

```
compile+compare 1293 tiles: 10.3s   (context 0.1s)
byte-exact files: 118/1293   dim-match: 1272/1293
cells: 95,773,940   diff: 276,839   accuracy: 99.7109%
```

**diff 计数与 Python/numba 版的存档数字(276839 / 95.7M)逐 cell 一致** — f64 内核
逐操作对齐的移植在全语料尺度上 cell-for-cell 等价。此 **vanilla 语料**数字不受下述
mod 修复影响(vanilla 无 Layout-Link-to-base / 克隆链,下面两处修复在全 1293 tile 上
逐字节零改动)。

### mod 瓦片 A/B + Layout-Link 修复(2026-07-12)

vanilla shipped `.mpp` 是多年前烘的、且原生本身非确定(见下),故对 **mod** 瓦片改用
**新鲜原生 bake 做 oracle**(`TL2-Mikuro-Console --clean --twice`),对 17 张挑战者
「通用素材01」ACT5 瓦片逐格 A/B。发现并按原生逻辑修复了两条 **MPP+LAYOUT 强相关链路**:

1. **Layout-Link 解析回退基础安装**(`resolve_link_layout` 加 `fallback_dir`,mod-over-base
   VFS,与碰撞 mesh 解析同款)。打 mod 时指向基础游戏 prop 布局的 link(如
   `CABIN_01_NOSPAWNER.LAYOUT`,只在安装里、不在 mod)此前解析失败 → 其碰撞不进烘焙
   → 整片 prop footprint W2w under-block(DARKWOODS 一处 2475-cell 空洞)。
2. **变换继承:Layout-Link 父不传变换**(`compute_world` 把已有的 Room-Piece 克隆记账
   规则扩展到 `Layout Link*`)。DARKWOODS 有 Layout-Link-under-Layout-Link 克隆链
   (每层 local scale 2~3,×72),原生烘焙 box=1100×1626 **要求**不继承(全继承会
   撑到 1776×2050)= 原生硬输出验证,非经验式。

结果(17 瓦片 vs 新鲜原生,aggregate):**99.7232% → 99.7718%**,12 张改善 0 张回归
(DARKWOODS −3171、DARKWOODS111 −3167、NETHER −2426);vanilla 全语料逐字节不变;
`compile-mpp` Rust 产物与 Python 参考实现在这 17 张 + 8 张 vanilla 上 **byte-exact**。

**原生非确定性 = 真实浮点天花板(实测)。** 同一 mod 瓦片连烘两次:DARKWOODS 的 region
box 会在 **1100 vs 1125** 之间抖动(±25 cell = 一个 10-unit snap-tile;某碰撞 bound 正好
卡在 snap 阈值上,float 噪声令部分 run 多进/少进一个 tile);同 box 的两次 bake 还有
~0.1–0.6% cell 抖动(cliff-oscillation)。**⇒ 对一个自身非确定的 oracle,byte-exact
本质不可能;native-vs-native diff 才是正确的地板标尺,不是 byte-exact-vs-单次-bake。**

**实际可走性已达浮点地板。** cell 语义:`0=可走 / 1=墙 / 0xFF=void`;寻路上 1 与 255
均不可走。按此「de-cosmetic」口径(1≡255)复测:aggregate **99.8934%**,THECROSSING
**99.8938% ≈ 其浮点地板 0.0965%**。raw-byte 残差里约一半是树簇内部的 255↔1 void/wall
标注差(单 tile 可走性无影响、仅 tile 缝隙处相关)+ 余为不可约浮点。**换言之:寻路网格
的实际可走性已与原生匹配到浮点地板 —— 这也是只保留离线后端的依据。**

前述 vanilla 语料残差(276839 cell)同属该信息论下限(原生 cliff/multichunk 不确定 +
per-instance 运行时 nocollide),换语言/精度都不移动它。

## 与 Python 版的实现对应

| Rust | Python | 说明 |
| --- | --- | --- |
| `src/rghash.rs` | `rghash.py` | RGHash(== EditorGuts sub_100CA9A0),按码点哈希 |
| `src/text.rs` | — | UTF-16 surrogatepass 等价层(`WStr = Vec<u32>` 码点串) |
| `src/bindat.rs` | `bindat.py` | 每文件 hash string-id(`HashStringDict`),排序/探测语义逐一对齐 |
| `src/binlayout.rs` | `binlayout.py` | 纯 DLL schema 以 `include_str!` 内嵌(`data/*.json`) |
| `src/raw.rs` | `raw.py` | 两种扫描序(files-first / interleaved)+ UNITDATA BASEFILE 继承链 |
| `src/packer.rs` | `packer.py` | 容器写入、rollingHash/mhash、manifest 树、convert_all 编排 |
| `src/binlayout_decompile.rs` | — | BINLAYOUT → LAYOUT 反编译(schema 反查;不动点 8986/8986) |
| `src/bindat_decompile.rs` | — | BINDAT → DAT 反编译(内嵌词表 + `UNK_<HEX8>` 转义) |
| `src/pak.rs` / `src/unpack.rs` | — | 容器读取(header/manifest/块)与 `.MOD`→源目录解包 |
| `src/tui.rs` | — | ratatui 交互界面(binary-only,不进 wasm) |
| `src/mpp/*` | `mpp/*` | region 头部 f32 舍入逐操作对齐;native 分类器 = `build_grid_fast` 等价(桶剔除,逐行 rayon 并行) |

刻意保留的 bug-for-bug 行为:ogre_mesh 顶点声明只读第一个 element(Python 的 +14
步进 bug,POSITION 恒为 element 0);binlayout 的 `NAME`/`CHOICE`/`GAME MODE` 首次出现
吞为对象元数据;TAGS.DAT 空根名哈希等 — 详见各文件注释与
`tl2-packer-formats-bindat-binlayout-mpp` 项目记忆。

未移植:`rules.py`(multichunk 分类只影响诊断信息,不影响 .mpp 字节)。

浏览器版(同一套 `#[path]` 共享代码编到 wasm)见 `../tl2-mikuro-mod-packer-wasm`。

## BINDAT 双 id 模式(2026-07-04 起)

对 EditorGuts 逐函数审计后,把 `sub_10289A40` 的**确切行为**也实现了(此前只有
per-file hash id 模式):

- **hash 模式(默认)**:string id = 每文件 RGHash + 线性探测。零语料依赖、天然并行、
  游戏按文件内表解析已验证。生产打包默认走这条。
- **corpus 模式**:string id = GUTS 全局 interner 的重建字典
  (`data/bindat_string_dict.bin`,154627 串,一次性加载 0.05s);未知串按**首现序**
  `max_id+1, +2, …` 分配 — 与 `sub_1023E9F0` 的 `id = counter++` 语义一致。
  产物与 shipped 基础游戏 **15976/16084 字节精确**(与 Python 参考实现同数;108 差异
  = 已知 shipped float quirks + TAGS.DAT,非实现问题)。
  开启:打包时设 `MIKURO_BINDAT_DICT=<dict.bin>`,或单文件
  `compile-dat <in> <out> --global-dict <dict.bin>`;
  验证:`verify bindat-corpus <media_dir> <dict.bin>`。

**性能**:corpus 模式反而略快(全语料 16084 文件编译 ~1.0s vs hash ~1.2-1.7s;
职业技能 22027 DAT 端到端 1.59s vs 1.75s)— hash 模式要付每文件的排序 BTreeSet +
探测成本,corpus 模式只是只读查表,且字典在 rayon worker 间零锁共享。**无性能回归**。

**为何默认仍是 hash**:corpus 字典携带 shipped 语料的 715 处 majority-vote id 碰撞
(两个不同字符串共享一个 id;同文件同时出现时表里后者覆盖前者)。hash 模式保证
文件内 id 唯一,是更安全的生产默认;corpus 模式用于与 GUTS 产物做字节级 diff/审计。
