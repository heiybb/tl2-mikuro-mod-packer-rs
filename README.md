# mikuro-mod-rs

Torchlight 2 `.MOD` 构建器 — `mikuro_mod_packer`(Python 版,位于
`D:\WizardProject\TL2\TL2-RE`)的 **Rust 移植**,目标是更快的构建速度与单文件可分发的
二进制。三种二进制格式(BINDAT / BINLAYOUT / MPP)都是真正的从零编译器:解析文本 →
序列化二进制,不读取任何已有二进制,因此文本改动总能正确编译。

## 验证状态(2026-07-04,与 Python 参考实现 A/B 对比)

| 模块 | 验证方式 | 结果 |
| --- | --- | --- |
| BINLAYOUT | 基础游戏全语料 8985 个 .LAYOUT 字节对比 shipped | **8965/8985 字节精确**(与 Python 完全同级;20 个 miss 是 shipped 文件里不可复现的未初始化 `@25` 垃圾) |
| BINDAT | 400 随机样本 + TAGS.DAT,vs Python `compile_dat` | **401/401 字节一致**(含非法 UTF-16 surrogate 的 TAGS.DAT) |
| RAW ×7 | vs 基础游戏 shipped `MEDIA/*.RAW` | **7/7 字节精确**(AFFIXES/SKILLS/TRIGGERABLES/MISSILES/UI/UNITDATA/ROOMPIECES) |
| MPP (offline `re`) | 13 个多样 tile(cliff/landmark 别名/boss/town/exit/cave/vault)vs Python `compile_mpp` | **13/13 字节一致** |
| .MOD 整包 | 佣兵系统 / 职业技能 / 通用素材01 三个真实 mod,vs Python 整包 | header+manifest 结构一致;**全部 1856 / 35864 / 11205 个文件解压内容、CRC、ftime 一致**;rollingHash + mhash 自洽(游戏校验的两个哈希) |

唯一与 Python 产物的字节差异 = zlib 压缩流(压缩后端不同;游戏只做 inflate,无感知,
且 Rust 产物略小)。

## 性能(16 核,同一台机器,产物内容一致)

| Mod | 规模 | Python(isal+numba+多进程) | Rust | 加速 |
| --- | --- | --- | --- | --- |
| 佣兵系统 | 65MB, 942 DAT + 160 LAYOUT + 5 MPP | 3.0s | **1.1s** | 2.7× |
| 职业技能 | 590MB, 22027 DAT + 10326 LAYOUT | 13.3s | **8.3s** | 1.6× |
| 通用素材01 | 264MB pak, 170 MPP | 18.2s | **7.8s** | 2.3× |

(Python 一侧已带满优化:isal SIMD 压缩、numba JIT 的 MPP 内核、进程/线程池;Rust
的收益主要来自零启动成本、rayon 全并行与无 IPC。首次冷启动时 Python 还要额外付
numba 编译等开销,差距更大。)

## 用法

```powershell
cargo build --release
# 与 Python CLI 同参数:
.\target\release\mikuro-mod.exe [--in-place|--temp-copy] [--mpp {re,dll,none}] [--raw {auto,none}] [--deploy] <mod目录>
# 单文件工具:
.\target\release\mikuro-mod.exe compile-dat    <in.DAT>    <out.BINDAT>
.\target\release\mikuro-mod.exe compile-layout <in.LAYOUT> <out.BINLAYOUT>
.\target\release\mikuro-mod.exe compile-mpp    <in.LAYOUT> <out.mpp> [--media <MEDIA>]
# 语料验证:
.\target\release\verify.exe binlayout|bindat|raw|mpp <media_dir> ...
```

- 默认(不加 flag):内存内转换 + 直接打包,不写源目录(等价 Python 默认 overrides 模式)。
- `--deploy`:输出 `<MOD_FILE_NAME>.MOD` 到 `%Documents%\My Games\Runic Games\Torchlight 2\mods`
  (注册表 Shell Folders 解析,OneDrive 重定向安全)。
- `--mpp re`(默认)离线寻路网格(≈99.7% cell 精确,已知天花板);`--mpp dll` 走
  `TL2-Mikuro-Console.exe` 驱动真实 EditorGuts.dll(字节精确,需可用的编辑器环境);
  `--mpp none` 保留源目录已有 .mpp。
- 环境变量:`TL2_MEDIA_DIR`(基础游戏 MEDIA,默认 `E:\Torchlight 2\MEDIA`)、
  `TL2_INSTALL_DIR`(dll 后端)、`TL2_MOD_GAMEVER`(覆写 header gamever)。

## 与 Python 版的实现对应

| Rust | Python | 说明 |
| --- | --- | --- |
| `src/rghash.rs` | `rghash.py` | RGHash(== EditorGuts sub_100CA9A0),按码点哈希 |
| `src/text.rs` | — | UTF-16 surrogatepass 等价层(`WStr = Vec<u32>` 码点串) |
| `src/bindat.rs` | `bindat.py` | 每文件 hash string-id(`HashStringDict`),排序/探测语义逐一对齐 |
| `src/binlayout.rs` | `binlayout.py` | 纯 DLL schema 以 `include_str!` 内嵌(`data/*.json`) |
| `src/raw.rs` | `raw.py` | 两种扫描序(files-first / interleaved)+ UNITDATA BASEFILE 继承链 |
| `src/packer.rs` | `packer.py` | 容器写入、rollingHash/mhash、manifest 树、convert_all 编排 |
| `src/mpp/*` | `mpp/*` | region 头部 f32 舍入逐操作对齐;native 分类器 = `build_grid_fast` 等价(桶剔除,逐行 rayon 并行);`dll.rs` = console host 驱动 |

刻意保留的 bug-for-bug 行为:ogre_mesh 顶点声明只读第一个 element(Python 的 +14
步进 bug,POSITION 恒为 element 0);binlayout 的 `NAME`/`CHOICE`/`GAME MODE` 首次出现
吞为对象元数据;TAGS.DAT 空根名哈希等 — 详见各文件注释与
`tl2-packer-formats-bindat-binlayout-mpp` 项目记忆。

未移植:`rules.py`(multichunk 分类只影响诊断信息,不影响 .mpp 字节);Python 端的
corpus StringDict 测试路径(生产路径本就不用 corpus 字典)。
