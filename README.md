# Oxipresso

Oxipresso is a Rust-first reimplementation of
[TeXpresso](https://github.com/let-def/texpresso) — a live TeX previewer that
typesets your document on every keystroke and displays the rendered pages in
real time.

## Features

- **Real XeTeX engine**: embeds the actual TeXpresso/XeTeX C engine via FFI,
- **Checkpoint hot rebuilds (resident passes)**: with `OXIPRESSO_RESIDENT=1` and a prebuilt format, editor changes restore the engine's S0 checkpoint and re-typeset in one pass without reloading the 22 MB format — measured ~2.7x faster than a full rebuild (~170-190ms vs ~496ms); the read set of each pass skips rebuilds for edits the engine never saw.
- **Package providers**: TeX Live via `kpsewhich` (default, with a persistent resolution cache) or `-tectonic` over a local Tectonic-style bundle directory (`OXIPRESSO_TECTONIC_BUNDLE`; network bundles are not supported).
  with automatic format bootstrap from `xelatex.ini` and TeX Live package
  resolution through `kpsewhich`.
- **Real glyph rendering**: XDV pages are rendered with FreeType-rasterized
  glyphs, including color specials (`rgb`/`gray`/`cmyk`/`hsb`), the slant/extend/
  embolden transform trio, and native OTF/Type1 font lookup from the TeX
  distribution.
- **Images**: `pdf:image` specials from `\includegraphics` and `\XeTeXpicfile`
  are parsed, PNGs decoded, and composited onto pages (alpha blending); editing
  an image in place correctly invalidates the cached render.
- **Live preview GUI**: `-gui` runs the engine, an egui window, and the editor
  wire (stdin/stdout) in one process — the same architecture as the original.
- **Editor theme**: a `(theme bg fg)` command sets the preview page background and
  default ink color (converted like the original), repainting cached pages.
- **Bidirectional SyncTeX**: forward search (source → PDF page) and reverse
  search (PDF click → source line) with sidecar capture and parsing.
- **Editor protocol**: TeXpresso-compatible S-expression and JSON wire protocol
  with `open`/`change`/`lookup-file`/`input-file`/`truncate`/`append`/`flush`
  and streamed `out`/`log` info buffers (plus `pause`/`resume`/`rescan`).
- **Interactive incremental rebuilds**: a format cache, a persistent `kpsewhich`
  resolution cache (≈25× faster warm rebuilds: 15.5s → ~600ms), `read_files`-based
  rebuild skipping (edits to files the engine never read cost nothing), and a
  content-hash page cache so unchanged XDV pages are reused — the preview stays
  responsive across edits.
- **Cross-platform architecture**: Rust owns the protocol, VFS, engine
  abstraction, platform layer, and rendering; the C engine is isolated behind
  a stable FFI shim. Windows is the primary tested target; Linux and macOS
  seams are preserved, and the pure-Rust core crates fully build (codegen to
  rlibs, not just type-check) for both `x86_64-unknown-linux-musl` and
  `x86_64-unknown-linux-gnu`.

## Quick start

### Live preview (real engine)

```powershell
# One-time: build the real engine (requires vcpkg + texpresso-src)
$env:OXIPRESSO_USE_REAL_XETEX = "1"
$env:TEXPRESSO_SRC = "F:\code\texpresso-src"
$env:VCPKG_ROOT = "F:\code\vcpkg"

# Live preview window
cargo run -p oxipresso-cli --features gui,freetype --bin oxipresso -- -gui doc.tex
```

The window shows the typeset pages. Your editor sends protocol commands over
stdin; every `change` triggers a rebuild and the window refreshes automatically.
Reverse SyncTeX clicks emit source locations over stdout.

### Headless (editor wire only)

```powershell
cargo run -p oxipresso-cli --bin oxipresso -- -stream -test-initialize doc.tex
```

### Reference editor client (Slint)

`oxipresso-editor-client` is a reference implementation of the editor side of
the wire — the integration contract an emacs/vscode plugin implements — as a
small Slint GUI. The engine runs as a child process; the client speaks the
protocol over stdin/stdout exactly like TeXpresso's plugins: the left pane
edits the document ("Send change" issues the whole-buffer `change` an editor
save produces), the right pane shows the engine's notices (truncate/append/
flush, input-file/lookup-file) and the mirrored `out` info buffer.

```powershell
cargo build -p oxipresso-cli --features slint
cargo run -p oxipresso-cli --features slint --bin oxipresso-editor-client doc.tex
# JSON wire form: add --json
```

The same contract is pinned headlessly by the cargo test
`editor_wire_selftest_against_shipped_binary` (part of `cargo test
--workspace`): it spawns the shipped `oxipresso` binary from outside the Rust
process and drives initialize → change-rebuild → pause/resume, asserting the
message shapes.

### Resident hot rebuilds (checkpoint-incremental)

With `OXIPRESSO_RESIDENT=1` (real engine + a prebuilt format file), editor
changes restore the engine's S0 checkpoint and re-typeset in a single pass
without reloading the 22 MB format — measured ~2.7x faster than a full
rebuild (`~170-190ms` vs `~496ms` on the simple fixture). Enable it on the
wire exactly like above; the binary handles the rest.

### PDF rendering (PDFium)

```powershell
cargo run -p oxipresso-cli --features gui,pdfium --bin oxipresso -- -gui doc.tex
```

## Architecture

```
Editor (Emacs/Vim/...)
  ↕ stdin/stdout (S-expression or JSON)
┌───────────────────────────────────┐
│ oxipresso-cli                     │
│  ├─ editor protocol parser        │
│  ├─ VFS (editor buffers + disk)   │
│  ├─ engine (XeTeX via FFI)        │
│  ├─ SyncTeX parser                │
│  └─ render backend                │
│    ├─ XDV glyph (FreeType)        │
│    ├─ PDF (PDFium)                │
│    └─ placeholder                 │
│  └─ egui live preview window      │
└───────────────────────────────────┘
```

The real XeTeX engine is compiled from a local `texpresso-src` checkout and
linked statically into the Rust binary via vcpkg dependencies (freetype,
harfbuzz, graphite2, fontconfig, icu, libpng, zlib).

## Workspace

| Crate | Purpose |
|-------|---------|
| `oxipresso-cli` | CLI entry point + live preview GUI |
| `oxipresso-editor-protocol` | Editor wire protocol (sexp + JSON) |
| `oxipresso-vfs` | Virtual file system (editor buffers + disk + resolver) |
| `oxipresso-engine-api` | Engine/VFS traits, `FileResolver`, shared types |
| `oxipresso-engine-xetex-sys` | C FFI shim (portable stub + real XeTeX engine) |
| `oxipresso-engine-xetex` | Safe Rust wrapper (bootstrap, SyncTeX, output events, TeX Live resolver) |
| `oxipresso-engine-external` | External `xelatex` process backend |
| `oxipresso-platform` | Platform isolation + file watcher |
| `oxipresso-render` | XDV/DVI parser + FreeType glyph + PNG image renderer + PDF (PDFium) |
| `oxipresso-synctex` | SyncTeX decoder + forward/reverse lookup |
| `oxipresso-viewer` | Viewer state model + standalone egui viewer |
| `oxipresso-testkit` | Shared fixtures and test helpers |

## Build

```powershell
# Default (stub engine, no native deps)
cargo test --workspace

# With real glyph rendering
cargo test -p oxipresso-render --features freetype

# With GUI
cargo check -p oxipresso-cli --features gui

# With everything
cargo check -p oxipresso-cli --features "gui,freetype,pdfium"

# Quality bar (both are expected clean)
cargo fmt --all --check
cargo clippy --workspace --all-targets
```

## Real engine setup

1. Clone [texpresso](https://github.com/let-def/texpresso) to `F:\code\texpresso-src`.
2. Install [vcpkg](https://vcpkg.io) and set `VCPKG_ROOT`.
3. Install the required packages:
   ```
   vcpkg install freetype harfbuzz[graphite2,icu] graphite2 fontconfig icu --triplet x64-windows-static-md
   ```
4. Build with `OXIPRESSO_USE_REAL_XETEX=1`.

The first run bootstraps the TeX format file (`texpresso.fmt`) from
`xelatex.ini`; subsequent runs load the cached format directly.

## License

MIT
