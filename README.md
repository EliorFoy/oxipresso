# Oxipresso

**中文** · [English](#english)

一个用 Rust 重实现的 **TeXpresso 式增量 TeX 预览服务器**：XeTeX 引擎常驻内存，通过检查点围栏实现毫秒级增量重排，配合流式页面推送，让 LaTeX 文档在编辑时近乎实时地刷新预览。

> 本项目是 [TeXpresso](https://github.com/let-def/texpresso)（Frédéric Bour，MIT 协议）的 Rust 重实现。架构设计、编辑器协议与增量模型均以原项目为参考；引擎部分在构建时直接编译原项目的 XeTeX 引擎源码。

## 特性

- **常驻会话增量重排**：引擎在检查点围栏驻留，编辑到来时从最近的合法检查点续排。S0（格式加载后）+ 自适应编辑点中段围栏两级检查点；文档尾部编辑实测 **52–80ms**，排版进行中的新编辑会被直接吸收进当前趟（一次击键爆发只花一趟排版）。
- **流式页面推送**：排版进行中每 250ms 推送已排好的页面，预览端即时上屏，不必等整篇排完。
- **XDV 增量渲染**：完整 XDV/DVI 解析器；经典 Type1 字体经 dvips map → .enc 编码向量正确解析（LM 数学字体、rm-* 复刻字体），原生 CJK 字体走字形索引；页面 digest 缓存让未变页面零成本复用；渲染密度自动匹配显示 DPI。
- **SyncTeX 双向同步**：正向（源码行 → 页面）与逆向（点击 → 源码位置）；支持 TeXpresso 的 `/<tag>` 扩展记录。
- **TeXpresso 兼容编辑器协议**：S-expression 与 JSON 双格式，全部动词。
- **TeX 发行版解析**：kpsewhich（TeX Live / TinyTeX）+ 持久化解析缓存；Tectonic 本地 bundle；原生文件监听。

## 实测性能

在 23KB 中文论文（ctex + 12 宏包，7 页）上：

| 操作 | 耗时 |
| --- | --- |
| 冷启动（格式加载 + 全文排版） | ≈ 2.5s |
| 文档尾部热编辑（快速续排） | **52–80ms** |
| 排版中的编辑吸收 | 0（并入当前趟） |
| 页面重渲染（缓存命中 / 内容变化） | 4ms / ~90ms |
| 首页上屏（编辑文档开头，流式） | ~0.5s |

## 架构

| crate | 职责 |
| --- | --- |
| `oxipresso-engine-xetex-sys` | XeTeX FFI shim：检查点围栏、池快照、驻留 pass 循环 |
| `oxipresso-engine-xetex` | 安全封装：驻留会话、编辑注入、快速续排、流式快照 |
| `oxipresso-render` | XDV 解析 + FreeType 字形渲染（map/.enc 字体映射、密度匹配、页面缓存） |
| `oxipresso-synctex` | SyncTeX 解析与正/反向查询 |
| `oxipresso-editor-protocol` | 编辑器 wire 协议（sexp/JSON） |
| `oxipresso-vfs` | 编辑器缓冲虚拟文件系统 |
| `oxipresso-cli` | wire 服务器（stdio）、驻留管线、`-gui` 宿主 |

GUI 客户端在独立仓库：[oxipresso-editor-client](https://github.com/EliorFoy/oxipresso-editor-client)。

## 构建

**纯 Rust stub 模式（零外部依赖）**：

```bash
cargo build --release -p oxipresso-cli
```

**真实引擎模式（增量预览需要）**：

前置条件：vcpkg（freetype/harfbuzz/icu 等，Windows 用 x64-windows-static-md）、TeX Live / TinyTeX（kpsewhich 在 PATH）、[TeXpresso](https://github.com/let-def/texpresso) 源码检出（提供 XeTeX 引擎）。

```powershell
$env:VCPKG_ROOT = "F:\code\vcpkg"
$env:OXIPRESSO_USE_REAL_XETEX = "1"
$env:TEXPRESSO_SRC = "F:\code\texpresso-src"
cargo build --release -p oxipresso-cli --features freetype
```

首次运行在 `OXIPRESSO_XETEX_FORMAT`（默认 `texpresso.fmt`）生成格式文件。

## 使用

```bash
oxipresso.exe -stream paper.tex
```

推荐配合 GUI 客户端：把 `oxipresso.exe` 与 `oxipresso-editor-client.exe` 放同一目录，运行客户端即可。

| 环境变量 | 作用 |
| --- | --- |
| `OXIPRESSO_RESIDENT=1` | 启用常驻会话（增量热编辑） |
| `OXIPRESSO_ARTIFACT_OUT=<file>` | 每趟 pass 后写出的 XDV 产物 |
| `OXIPRESSO_SYNCTEX_OUT=<file>` | 每趟 pass 后写出的 SyncTeX sidecar |
| `OXIPRESSO_XETEX_FORMAT=<file>` | 引擎格式文件 |

## 与原项目的关系

[TeXpresso](https://github.com/let-def/texpresso) 证明了"编辑即所见"的 LaTeX 预览是可能的：引擎进程在每个输入读取围栏上 fork，写时复制快照让重放几乎零成本。本项目用 Rust 重实现该架构：协议、VFS、渲染、平台层全部为 Rust 原生实现，引擎通过 FFI 复用原项目的 XeTeX 源码，并把 fork/COW 模型改写为跨平台的检查点围栏 + 流式推送。设计文档见 `docs-latency-design.md`。

衷心感谢 Frédéric Bour 与 TeXpresso 的贡献者们。

## 许可证

本仓库的 Rust 代码以 [MIT](LICENSE) 协议发布。注意：以 `OXIPRESSO_USE_REAL_XETEX=1` 构建的二进制静态包含了 TeXpresso/XeTeX 引擎源码（GPL-2.0-or-later 及 XeTeX 例外条款），此类二进制的分发需遵循相应 GPL 条款；纯 stub 构建不包含任何引擎代码。

---

<a id="english"></a>

# Oxipresso (English)

A **TeXpresso-style incremental TeX preview server**, reimplemented in Rust: the XeTeX engine stays resident in memory, checkpoint fences make re-typesetting after an edit a millisecond-scale operation, and pages stream to the preview as they are shipped — LaTeX editing feels live.

> This project is a Rust reimplementation of [TeXpresso](https://github.com/let-def/texpresso) (Frédéric Bour, MIT). The architecture, editor protocol, and incremental model follow the original; the engine is built from TeXpresso's XeTeX sources at compile time.

## Features

- **Resident incremental re-typesetting**: the engine parks at checkpoint fences; edits resume from the nearest legal checkpoint. Tail edits measure **52–80ms**, and edits arriving mid-pass are absorbed into the running pass (a typing burst costs one pass).
- **Streaming partial snapshots**: pages are pushed every 250ms while the pass runs, so the preview updates progressively.
- **Incremental XDV rendering**: a full XDV/DVI parser; classic Type1 fonts resolve through dvips map → `.enc` encoding vectors (LM math fonts, `rm-*` replicas), native CJK fonts by glyph index; unchanged pages are reused by content digest; render density matches the display DPI.
- **Bidirectional SyncTeX**: forward (source line → page) and reverse (click → source position), including TeXpresso's `/<tag>` extension records.
- **TeXpresso-compatible editor protocol**: S-expression and JSON, all verbs.
- **TeX distribution support**: kpsewhich with a persistent resolution cache, local Tectonic bundles, a native file watcher.

## Measured performance

On a 23KB Chinese paper (ctex + 12 packages, 7 pages):

| Operation | Latency |
| --- | --- |
| Cold start (format load + full typeset) | ≈ 2.5s |
| Hot edit near the document tail (fast resume) | **52–80ms** |
| Edits absorbed mid-pass | 0 (joined the running pass) |
| Page re-render (cache hit / changed content) | 4ms / ~90ms |
| First page on screen (edit near the top, streaming) | ~0.5s |

## Architecture

| crate | role |
| --- | --- |
| `oxipresso-engine-xetex-sys` | XeTeX FFI shim: checkpoint fences, pool snapshots, the resident pass loop |
| `oxipresso-engine-xetex` | Safe wrappers: resident sessions, edit injection, fast resume, streaming snapshots |
| `oxipresso-render` | XDV parsing + FreeType glyph rendering (map/.enc mapping, density matching, page caching) |
| `oxipresso-synctex` | SyncTeX parsing and forward/reverse lookup |
| `oxipresso-editor-protocol` | The editor wire protocol (sexp/JSON) |
| `oxipresso-vfs` | Editor-backed virtual file system |
| `oxipresso-cli` | The wire server (stdio), the resident pipeline, the `-gui` host |

The GUI client lives in its own repository: [oxipresso-editor-client](https://github.com/EliorFoy/oxipresso-editor-client).

## Build

**Pure-Rust stub mode (no external dependencies)**:

```bash
cargo build --release -p oxipresso-cli
```

**Real-engine mode (required for live preview)**:

Prerequisites: vcpkg (freetype/harfbuzz/icu; `x64-windows-static-md` on Windows), TeX Live / TinyTeX on PATH, and a checkout of [TeXpresso](https://github.com/let-def/texpresso) providing the XeTeX engine sources.

```powershell
$env:VCPKG_ROOT = "F:\code\vcpkg"
$env:OXIPRESSO_USE_REAL_XETEX = "1"
$env:TEXPRESSO_SRC = "F:\code\texpresso-src"
cargo build --release -p oxipresso-cli --features freetype
```

The first run generates the engine format at `OXIPRESSO_XETEX_FORMAT` (default `texpresso.fmt`).

## Usage

```bash
oxipresso.exe -stream paper.tex
```

Pair it with the GUI client: place `oxipresso.exe` and `oxipresso-editor-client.exe` in the same directory and start the client.

| variable | purpose |
| --- | --- |
| `OXIPRESSO_RESIDENT=1` | enable the resident session (incremental hot edits) |
| `OXIPRESSO_ARTIFACT_OUT=<file>` | the XDV artifact written after every pass |
| `OXIPRESSO_SYNCTEX_OUT=<file>` | the SyncTeX sidecar written after every pass |
| `OXIPRESSO_XETEX_FORMAT=<file>` | engine format file |

## Relation to TeXpresso

[TeXpresso](https://github.com/let-def/texpresso) proved that "edit = see" LaTeX previewing is possible: the engine process forks at every input read fence, and copy-on-write snapshots make replay nearly free. This project reimplements that architecture in Rust — protocol, VFS, rendering, and the platform layer are native Rust; the engine is reused through FFI from TeXpresso's XeTeX sources; and the fork/COW model is recast as cross-platform checkpoint fences + streaming pushes. See `docs-latency-design.md` for the design notes.

Heartfelt thanks to Frédéric Bour and the TeXpresso contributors.

## License

The Rust code in this repository is released under the [MIT](LICENSE) license.

Note: binaries built with `OXIPRESSO_USE_REAL_XETEX=1` statically include the TeXpresso/XeTeX engine sources (GPL-2.0-or-later with the XeTeX additional exceptions); distributing such binaries is subject to those GPL terms. Stub-mode builds contain no engine code.