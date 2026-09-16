# Oxipresso

Oxipresso is a Rust-first reimplementation scaffold for
[TeXpresso](https://github.com/let-def/texpresso).

The current milestone establishes the cross-platform Rust architecture:

- editor protocol parsing and serialization for S-expression and JSON modes;
- editor-backed virtual file system with byte, line, and UTF-16 range changes;
- engine traits that isolate the Rust driver from concrete TeX backends;
- a first live rebuild loop that full-restarts the selected engine after editor
  changes and refreshes the produced artifact/viewer state;
- SyncTeX artifact capture and basic SyncTeX metadata parsing;
- a temporary XeTeX FFI crate with a portable C ABI stub;
- a first artifact-to-viewer pipeline for PDF metadata, PDFium-backed PDF raster
  rendering, and placeholder fallback;
- an optional `eframe/egui` + `wgpu` viewer shell for artifact preview and
  polling-based artifact refresh;
- a platform file-watcher abstraction with a portable polling implementation
  used by the GUI artifact watcher today;
- platform, render, viewer, and testkit crates ready for Windows-first work
  while preserving Linux and macOS seams.

The FFI crate intentionally does not yet embed the real TeXpresso/XeTeX
sources by default. Its `oxipresso_xetex_run` shim is the stable point where the real
engine can be connected without changing the Rust protocol, VFS, CLI, or viewer
layers.

An opt-in real engine mode now compiles and links the actual TeXpresso/XeTeX
engine (from a local `texpresso-src` checkout) behind the same shim on Windows:

```powershell
$env:OXIPRESSO_USE_REAL_XETEX = "1"
$env:TEXPRESSO_SRC = "F:\code\texpresso-src"   # original repository checkout
$env:VCPKG_ROOT = "F:\code\vcpkg"              # freetype, harfbuzz[graphite2,icu], graphite2, fontconfig, icu, libpng, zlib
cargo test -p oxipresso-engine-xetex --no-run  # real engine links into the Rust binary
```

Real mode pins the vcpkg `x64-windows-static-md` triplet so the engine is
statically linked. The real engine still needs a TeX format file
(`OXIPRESSO_XETEX_FORMAT`) to typeset, so default builds keep using the
dependency-free portable stub.

There is also an optional external `xelatex` backend for smoke-testing real
LaTeX compilation while the FFI backend is still being connected:

```powershell
$env:OXIPRESSO_ENGINE = "external"
$env:OXIPRESSO_ARTIFACT_OUT = "out.pdf"
cargo run -p oxipresso-cli -- -test-initialize path\to\main.tex
Remove-Item Env:\OXIPRESSO_ENGINE
Remove-Item Env:\OXIPRESSO_ARTIFACT_OUT
```

When `OXIPRESSO_ARTIFACT_OUT` is set, the current engine artifact is written to
that path. With the external backend this is currently a PDF. The CLI also
feeds produced artifacts into the internal viewer state so the GUI layer can
reuse the same render metadata path later.

When `OXIPRESSO_SYNCTEX_OUT` is set, the CLI writes the current backend's
SyncTeX artifact to that path. The external `xelatex` backend captures
`.synctex.gz` or `.synctex` files, and `oxipresso-synctex` can decode gzip/plain
SyncTeX enough to parse `Output:`, `Input:`, page sheet records, and common
node coordinates. It can perform a first forward lookup from source path/line to
a nearest SyncTeX hit. SyncTeX path matching normalizes backslashes, duplicate
slashes, and `./`, and supports absolute/relative suffix matching in either
direction for editor and engine path variants. The CLI uses that lookup to
update its internal viewer page for `synctex-forward`, and the optional GUI can
draw a lightweight marker
for the hit on the current page and request scrolling to it. The parser also
has a nearest-record reverse lookup by page and point. The marker currently
uses an approximate TeX-point-to-page mapping; full TeXpresso-compatible
source/PDF behavior is still pending.

`oxipresso-render` validates PDF headers, counts visible `/Type /Page` objects
for metadata, walks DVI/XDV opcodes far enough to count `bop` pages, and returns
stable placeholder pages by default. It also has a first DVI/XDV display-list
increment: `set_rule` and `put_rule` opcodes are parsed with common movement and
stack commands, then painted as dark rectangles on the placeholder page. Basic
glyph opcodes (`set_char`, `set1..4`, and `put1..4`) are also collected and
painted as placeholder marks, so DVI/XDV pages can now show rough text
positions before real font rendering exists. Font definitions and font
selection are parsed enough to size those glyph placeholders from the current
font's scaled size. With the optional `pdfium` feature, it can render real PDF
pages through PDFium. Real glyph/font/image rendering for XDV/DVI is still
pending.

The optional GUI viewer can open an existing artifact:

```powershell
cargo run -p oxipresso-cli --features gui --bin oxipresso-viewer -- path\to\out.pdf
```

It can also watch an artifact path and refresh when another process rewrites the
file. This currently uses the `oxipresso-platform` polling watcher so the GUI
does not depend on Windows-only APIs; native Windows/Linux/macOS watcher
backends can replace that implementation later without changing viewer code:

```powershell
cargo run -p oxipresso-cli --features gui --bin oxipresso-viewer -- --watch out.pdf
```

One temporary live-preview workflow is to run the watcher above, then compile
with the external backend using the same output path:

```powershell
$env:OXIPRESSO_ENGINE = "external"
$env:OXIPRESSO_ARTIFACT_OUT = "out.pdf"
cargo run -p oxipresso-cli -- -test-initialize path\to\main.tex
```

The GUI is a shell over the current render backend. Enable `pdfium` to preview
real PDF pixels; without it, PDF pages use placeholder pixels.

The VFS now searches editor buffers first, then configured disk roots derived
from the root document and `-I` include paths. It records `input-file` events
for successfully opened inputs and can snapshot editor/disk input files into
the external `xelatex` backend's temporary build directory, so edited includes
can participate in the temporary full-restart live workflow.

Stream mode is covered by Rust tests for the first TeXpresso-compatible flows:
`register`, then `open`, then `resume` initializes the engine from editor VFS
content and emits the expected lookup/input messages. A registered missing
include can also become a promised lookup on resume; when the editor later
opens that file, the CLI rebuilds and the engine observes it as a successful
input. More async lookup snapshot tests from the original shell suite are still
pending.

The VFS also remembers files requested through failed engine reads. If the
editor later provides such a file with `open`, the CLI treats it as a change
from offset 0 and performs a rebuild. This covers the first non-blocking
`lookup-file failed` workflow from the original shell tests; full async protocol
snapshots are still pending.

TeX errors from the selected engine are treated as live diagnostics during
initialization and rebuild: when the backend reports diagnostics, the CLI emits
the corresponding protocol messages instead of terminating the session.

## Build

```powershell
cargo test --workspace
cargo test -p oxipresso-viewer --features gui
cargo test -p oxipresso-cli --features gui --bin oxipresso-viewer
cargo check -p oxipresso-cli --features gui --bin oxipresso-viewer
cargo check -p oxipresso-cli --features "gui pdfium" --bin oxipresso-viewer
cargo run -p oxipresso-cli -- -test-initialize path\to\main.tex
```

For a real PDFium raster smoke, point `OXIPRESSO_PDFIUM_SMOKE_PDF` at a PDF:

```powershell
$env:OXIPRESSO_PDFIUM_SMOKE_PDF = "path\to\out.pdf"
cargo test -p oxipresso-render --features pdfium pdfium_smoke_renders_real_pdf_when_requested
Remove-Item Env:\OXIPRESSO_PDFIUM_SMOKE_PDF
```

Some smoke tests can use fixtures from a local TeXpresso checkout. Set
`TEXPRESSO_SRC` to that checkout, or place it beside this repository as
`texpresso-src`; tests skip those fixture cases when the source tree is absent.

## Workspace

- `oxipresso-cli`: command-line entry point compatible with TeXpresso flags.
- `oxipresso-editor-protocol`: editor command/message wire protocol.
- `oxipresso-vfs`: virtual file system and `EngineIo` implementation.
- `oxipresso-engine-api`: engine and VFS traits shared across backends.
- `oxipresso-engine-xetex-sys`: native C ABI shim for the temporary XeTeX backend.
- `oxipresso-engine-xetex`: safe Rust wrapper around the XeTeX FFI boundary.
- `oxipresso-engine-external`: optional `xelatex` process backend for real PDF smoke tests.
- `oxipresso-platform`: Windows/Linux/macOS platform isolation plus file watcher abstraction.
- `oxipresso-render`: document artifact rendering abstractions plus PDF metadata support.
- `oxipresso-synctex`: SyncTeX gzip/plain decoder and metadata parser.
- `oxipresso-viewer`: viewer state model and optional `eframe/egui` viewer shell.
- `oxipresso-testkit`: shared fixtures and test helpers.
