# Oxipresso Agent Handoff

## Current Goal

Rust-first reimplementation scaffold for TeXpresso in `F:\code\oxipresso`.

Platform policy:

- Windows is the first implementation and test target.
- Linux must remain supported architecturally from the start, but is not the first tested target.
- macOS comes last.

Engine policy:

- Do not try to reimplement the full TeX/XeTeX engine immediately.
- Temporarily reuse XeTeX through an FFI boundary.
- Rust owns the protocol, VFS, engine abstraction, platform layer, rendering/viewer abstraction, and tests.
- The current XeTeX FFI is only a portable stub, not the real TeXpresso/XeTeX engine yet.
- The FFI stub now exercises the callback path: it opens and reads the root file through `EngineIo`, marks bytes as seen, and writes a stub stdout message through `EngineIo`.
- The FFI boundary has been extended for the real XeTeX shim plan: config now carries `format_path` and `build_date`; callbacks now include `size`, `flush`, and `diagnostic`.
- The default backend still compiles the portable C stub unless `OXIPRESSO_USE_REAL_XETEX=1` is set.
- An opt-in real shim source and build path now exist in `oxipresso-engine-xetex-sys`, and with vcpkg dependencies installed the real TeXpresso/XeTeX engine **compiles and links** into the Rust test binaries on Windows.
- Windows vcpkg dependencies live in `F:\code\vcpkg` (freetype, harfbuzz[graphite2,icu], graphite2, fontconfig, icu, libpng, zlib plus brotli/bzip2/expat/dirent), installed for both `x64-windows` and `x64-windows-static-md` triplets; real mode pins `x64-windows-static-md` (static libs, dynamic CRT) so the engine links into the Rust binary without runtime DLLs.
- An optional external `xelatex` backend now exists for real PDF smoke tests while the FFI backend is still being connected. Enable it with `OXIPRESSO_ENGINE=external`.
- The real engine now runs end to end: `XetexEngine::initialize` bootstraps `texpresso.fmt` in INI mode from the format source (default `xelatex.ini`) when the format file is missing, persists it to `OXIPRESSO_XETEX_FORMAT`, and then typesets the root document, producing real XDV artifacts through `EngineIo`.
- The real shim enables SyncTeX for normal runs (`synctex_enabled=1`, plain text via `synctex_use_gz=0`): the engine's `<jobname>.synctex` write is captured through the output mirror, exposed through `XetexEngine::output_synctex()`, and feeds the existing CLI `synctex-forward`/`OXIPRESSO_SYNCTEX_OUT` plumbing.
- The engine reports incremental output-stream writes through `TypesettingEngine::take_output_events()` (`OutputEvent { path, offset, data }` in `oxipresso-engine-api`), and the CLI maps them onto the editor info buffers exactly like the original protocol: stdout → `out`, the `.log` file → `log`, with `(truncate out/log 0)` at run start and `(flush)` at the end. Diagnostics summary messages are suppressed when stream events exist (the real engine already streams the same content through stdout/log).
- Incremental render foundation: `XdvDocument::page_digest` hashes each page's geometry, referenced font definitions, glyphs (codes + absolute positions), and rules; `XdvGlyphRenderBackend` caches rendered pages by digest across rebuilds, so pages whose XDV content is unchanged after an edit are reused instead of re-rendered. This is the renderer-side half of the original's incremental model.
- TeX distribution files are resolved through a new TeX Live provider: the VFS gained a `FileResolver` hook (defined in `oxipresso-engine-api`) and the CLI installs `KpsewhichResolver` (kpsewhich from TeX Live/TinyTeX) with editor buffers and disk roots still taking precedence. Resolution results (positive and negative) persist to a disk cache (`%TEMP%/oxipresso-kpsewhich-cache.txt`, overridable via `OXIPRESSO_KPSE_CACHE`), eliminating the ~100-200ms process spawn per lookup on warm rebuilds: measured 15.5s → ~600ms (25x) for a 3-section document with 55 cached lookups.
- CLI can persist the current engine artifact when `OXIPRESSO_ARTIFACT_OUT` is set. With the external backend, this writes a real PDF.
- Produced engine artifacts are now also passed through an internal viewer metadata pipeline.
- An optional `eframe/egui` + `wgpu` GUI shell now exists for opening and polling artifact files.
- GUI artifact watching now goes through an `oxipresso-platform` file-watcher abstraction. The `file_watcher()` factory now returns `Box<dyn FileWatcher>` (callers hold the trait object, so a native backend can be swapped in without touching the viewer), with the current backend being portable polling.
- PDF rendering can use real PDFium rasterization when the optional `pdfium` feature is enabled; otherwise it falls back to placeholder pages.
- XDV/DVI artifacts can now be rendered with **real glyphs**: the optional `freetype` feature adds a full XDV parser (`oxipresso-render::xdv`), a layout-independent FreeType binding (`ft`), and `XdvGlyphRenderBackend`, which resolves font files through a `FontResolver`, rasterizes glyphs with FreeType, and composites them onto RGBA pages.
- XDV color specials are rendered: the parser tracks `color push rgb/gray/cmyk/hsb` / `color pop` (from the LaTeX color package), splits glyph runs on color changes, and carries per-run `color_rgba`; CMYK uses the multiplicative model and `hsb` a proper HSV hexcone conversion (both were earlier approximated wrong). The glyph renderer applies element color over the font's own XDV color. Page digests include run colors so color edits invalidate correctly. The `-gui` window also completes click-to-source: clicking the page emits the reverse SyncTeX notification over the editor wire.
- XDV slant/extend transforms are applied as bitmap post-processing (`apply_slant`/`apply_extend` in the glyph backend) rather than through FT_Set_Transform, because MSVC's `FT_Pos` (long) is 4 bytes on Windows, which invalidates all FreeType struct offsets we rely on. The runtime path is pure Rust bitmap shearing/scaling. Embolden is applied via bitmap dilation (`apply_embolden`), completing the slant/extend/embolden transform trio. **Slant clip fixed (round 94):** `apply_slant` previously shifted every row by `max_shift` and only widened the canvas by `max_shift`, so a positive slant clipped the top-row overhang (half the ink lost for a solid block) and a negative slant clipped the left; it now sizes the canvas from the full `[shift_min, shift_max]` shear span and left-pads for negative slant, preserving all ink. Golden-tested by `slant_shears_right_without_dropping_ink`, `slant_negative_preserves_ink`, `extend_widens_and_scales_left`, `embolden_dilates_to_square_footprint`.
- Bidirectional SyncTeX is wired end to end: `OxipressoApp::synctex_reverse_message(page, x_pt, y_pt)` maps a viewer click (points) onto the SyncTeX sp coordinate space through reverse search and produces the engine-to-editor `synctex` notification; the viewer GUI auto-loads the `.synctex` sidecar next to the artifact and resolves clicks to source locations (marker + status line), using page dimensions parsed from the XDV.
- Editor integration path realized: `-gui` runs the engine, an egui live-preview window, and the editor wire (stdin/stdout) in one process — the TeXpresso architecture. Editor commands are pumped from stdin through the protocol app every frame; engine artifacts refresh the window directly; page navigation/zoom live in the toolbar. Requires the `gui` feature (`cargo run -p oxipresso-cli --features gui --bin oxipresso -- -gui doc.tex`).

## Completed Work

- Created a Rust Cargo workspace in `F:\code\oxipresso`.
- Added `.gitignore` with `/target/`.
- Added `README.md` describing current architecture and build commands.
- Added `Cargo.lock` and verified the workspace builds on Windows.

Implemented crates:

- `oxipresso-engine-api`
  - Defines `TypesettingEngine`.
  - Defines `EngineIo`.
  - `EngineIo` now includes `size(handle)` so XeTeX-style input handles can query file sizes without escaping the VFS boundary.
  - Defines shared types: `RootDocument`, `EngineEvent`, `RestartPolicy`, `DocumentArtifact`, `SyncTexArtifact`, `Diagnostic`, `FileKind`, `FileHandle`, `OpenResult`, `PictureKey`.
  - Adds the `FileResolver` trait for external file resolution fallbacks (e.g. TeX Live providers).
  - Adds default `TypesettingEngine::output_synctex()` for optional SyncTeX sidecar artifacts.

- `oxipresso-editor-protocol`
  - Parses S-expression editor commands.
  - Parses JSON editor commands.
  - Malformed/untrusted input is rejected with an error, never a panic: all field reads go through `expect_*`/`expect_arity` (returning `Err`), array indexing is guarded by arity checks, and `.expect()` is used only on infallible serialization of the parser's own types. Locked by `parsers_reject_malformed_input_without_panicking` (sexp + json malformed cases + a valid control).
  - Serializes editor messages as S-expression or JSON.
  - Supports first-stage commands including `open`, `open-base64`, `close`, `change`, `change-lines`, `change-range`, `register`, `pause`, `resume`, page/window/theme commands, and SyncTeX forward command.

- `oxipresso-vfs`
  - Implements editor-backed virtual file system.
  - Supports path normalization using forward slashes.
  - Supports `open`, `open-base64`, `close`, `change`, `change-lines`, `change-range`.
  - `change-lines` line→byte semantics verified against TeXpresso `main.c` (BASE_LINE): 0-based newline-counted start, `remove` consumes that many newlines inclusive, and dropping past an unterminated final line clamps to EOF (tolerated) while ≥2-short errors — pinned by `change_lines_matches_texpresso_line_semantics`.
  - `open-base64` decodes with the base64 `STANDARD` engine (canonical alphabet **with required `=` padding**); malformed or unpadded payloads are rejected with an error, never a panic or a silent mis-decode (tested by `open_base64_command_decodes_payload_or_errors`).
  - Implements UTF-16 code unit to UTF-8 byte offset conversion for `change-range` — matching TeXpresso `main.c` `BASE_RANGE` (columns are UTF-16 code units relative to each endpoint's line; multi-line ranges use the absolute end-line start), pinned by `change_range_spans_multiple_lines_with_utf16_columns`.
  - Tracks promised files.
  - Opening a promised file through the editor now reports a change at offset 0 so promised-file fulfillment can trigger a rebuild. (Deliberate adaptation from TeXpresso `interpret_open`, which on a promised file's first open does *not* notify — its engine is blocked on that very read. Our non-blocking FFI has no such block, so the change event drives the rebuild; exercised by the register/deferred + non-blocking-lookup CLI tests.)
  - Tracks files that were requested by engine reads, including failed lookup attempts.
  - Opening a file after a failed engine lookup now reports a change at offset 0 so non-blocking `lookup-file failed` workflows can trigger a rebuild when the editor provides the file.
  - Supports disk-root fallback search after editor/disk cached content. CLI seeds disk roots from the root directory plus `-I` include paths.
  - Supports an external `FileResolver` hook consulted after editor buffers, disk roots, and absolute paths fail; resolved content is cached as disk data and participates in lookup/input-file events.
  - Editor buffer content takes precedence over disk-root content for the same normalized path.
  - Records `input-file` events once per successfully opened input path.
  - Exposes `snapshot_inputs()` through `EngineIo` so process-style engines can materialize the current editor-backed input set.
  - Records lookup events when an engine backend opens files for reading or writing.
  - Implements `EngineIo` for future engine backends.
  - Implements `EngineIo::size()` for both read handles and write handles.

- `oxipresso-engine-xetex-sys`
  - Adds a C ABI surface for temporary XeTeX integration.
  - Builds a portable C stub through the `cc` crate.
  - `build.rs` branches for Windows, Linux, and macOS targets.
  - Default build behavior remains stub-only and does not require vcpkg/pkg-config/native TeX dependencies.
  - Adds opt-in real engine mode behind `OXIPRESSO_USE_REAL_XETEX=1`.
  - Real mode locates original sources through `TEXPRESSO_SRC` or defaults to `F:\code\texpresso-src`.
  - Real mode compiles a new `c/oxipresso_xetex_real.c` shim plus original `src/engine/engine`, `src/engine/layout`, `src/engine/dpx`, and `main/zlib_md5.c`, while excluding `main/main.c`, `main/fork.c`, and `main/texpresso_protocol.c`.
  - Real mode now explicitly excludes macOS-only source files from non-macOS builds, including `xetex-macos.c`, `xetex-XeTeXFontInst_Mac.cpp`, and `xetex-XeTeXFontMgr_Mac.mm`.
  - Real mode keeps Windows dependency discovery on vcpkg, Linux on pkg-config, and macOS as an explicit later placeholder.
  - Real mode Windows discovery uses vcpkg port names (`freetype`, `icu`) instead of pkg-config names (`freetype2`, `icu-uc`), and pins the `x64-windows-static-md` triplet (static libs, dynamic CRT) so the engine links into the Rust binary without runtime DLLs.
  - Real mode compiles C++ sources with `/std:c++17` through cc-rs `std()`; a raw `-std=c++17` flag is rejected by `cl.exe` and silently dropped by `flag_if_supported`.
  - Real mode links `advapi32` on Windows for ICU's registry-based Windows time-zone detection.
  - With `VCPKG_ROOT=F:\code\vcpkg` the full real engine (engine + layout + dpx + shim) compiles and links into `oxipresso-engine-xetex` test binaries on Windows.
  - Exposes `oxipresso_xetex_run`.
  - Current C stub calls `open_read`, `read`, `size`, `seen`, `close`, `open_write`, `append`, `flush`, and `diagnostic` callbacks.
  - Current C stub writes a small `<root-stem>.xdv` artifact so Rust output mirroring is covered by default tests.
  - New real shim implements first-pass `ttstub_*` bridge functions for input cursoring, output callbacks, diagnostics, disabled shell escape, primary input, format input, and abort capture through `setjmp`/`longjmp`.
  - New real shim now opens format outputs with `TTBC_FILE_FORMAT_FORMAT` and reports full input size as seen on close when possible.
  - The real shim mirrors the original engine main's input behavior: failed missing-file opens are retried with per-format extensions (`xelatex` -> `xelatex.tex`), the config carries `primary_name` and `in_initex_mode`, `ttstub_input_open_primary` opens the format source as a TEX input during bootstrap, and `tt_xetex_set_int_variable` configures `in_initex_mode` plus `halt_on_error_p` per run.
  - Both shims expose `oxipresso_xetex_is_real()` so the Rust wrapper can detect stub vs. real at runtime.

- `oxipresso-engine-xetex`
  - Adds safe Rust wrapper `XetexEngine`.
  - Implements `TypesettingEngine`.
  - Reports engine name `xetex-ffi-stub` in stub mode and `xetex-real` in real mode; `XetexEngine::real_mode()` exposes the linked shim kind.
  - Bootstraps the format in real mode: when `OXIPRESSO_XETEX_FORMAT` is missing, `initialize` runs the engine in INI mode with the format source (`OXIPRESSO_XETEX_FORMAT_SOURCE`, default `xelatex.ini`) as primary input, requires a spotless run plus nonempty format output, and persists the produced `texpresso.fmt` to disk before the normal typesetting run.
  - Adds the `texlive` module with `KpsewhichResolver`, a `FileResolver` implementation that resolves TeX distribution files through `kpsewhich` (TeX Live/TinyTeX), with negative-lookup caching and no resolution for engine-owned formats or the editor primary document.
  - Auto-configures fontconfig before engine runs: `FONTCONFIG_PATH` discovery (existing env → `OXIPRESSO_FONTCONFIG_PATH` → vcpkg `etc/fonts`), set via `oxipresso_platform::set_process_env` which also mirrors into the MSVC CRT block via `_wputenv_s` on Windows (plain `env::set_var` is invisible to C `getenv` callers), eliminating the "Cannot load default config file" runtime error. The OS-specific CRT quirk lives only in `oxipresso-platform`, keeping `oxipresso-engine-xetex` free of `#[cfg(windows)]`.
  - Serializes engine invocations through a process-wide lock: the C shim keeps non-reentrant global session state, so parallel callers (tests, future threads) cannot race it.
  - Bridges FFI callbacks into Rust `EngineIo`.
  - Passes `OXIPRESSO_XETEX_FORMAT` to the FFI config, defaulting to `texpresso.fmt`.
  - Captures FFI diagnostics into Rust `Diagnostic` values.
  - Mirrors output bytes by write handle/path during callbacks.
  - Selects produced artifacts from mirrored `<root-stem>.xdv`, `<root-stem>.pdf`, then `<root-stem>.dvi`.
  - Maps original `TTBC_FILE_FORMAT_*` numeric values into Rust `FileKind` instead of treating all FFI lookups as `Other`.
  - Has a test verifying that the FFI stub reads `simple.tex`, updates `seen_offset`, and writes stdout through the VFS.
  - Has a test verifying that the FFI stub output mirror produces a stub XDV `DocumentArtifact`.
  - Has a test for mapping key Tectonic file-format enum values to Rust `FileKind`.
  - Has an opt-in real XeTeX smoke test that runs only when `OXIPRESSO_USE_REAL_XETEX=1`, `TEXPRESSO_SRC`, and `OXIPRESSO_XETEX_FORMAT` are all set.
  - `apply_change_hint` returns `NoRestartNeeded` when the changed file was never opened by the engine during the last run (tracked via a `read_files` set populated from `open_read` callbacks), avoiding unnecessary rebuilds for unrelated file edits. Returns `FullRestartRequired` when the engine actually read the changed file. Read-path and change-hint paths are compared on a canonical key (forward slashes, no leading `./`) via `path_key`, so a Windows-separator engine path still matches the VFS-normalized change hint (a mismatch would otherwise wrongly skip a needed rebuild and show a stale preview); relative and absolute forms are deliberately not conflated (tested by `change_hint_matches_engine_path_regardless_of_separator`).

- `oxipresso-engine-external`
  - Adds an optional external `xelatex` process backend.
  - Reads the root document through `EngineIo`.
  - Materializes `EngineIo::snapshot_inputs()` into the temporary build directory before invoking `xelatex`, allowing edited include files to participate in external full-restart compiles.
  - Sanitizes snapshot paths and ignores absolute or parent-directory paths when writing into the temporary build directory.
  - Writes the root file to a temporary build directory.
  - Invokes `xelatex` with nonstop/halt-on-error/file-line-error/synctex flags.
  - Sets `TEXINPUTS` from root dir, build dir, `RootDocument.include_paths`, and the existing environment while preserving TeX defaults.
  - Produces a PDF `DocumentArtifact` when compilation succeeds.
  - Captures `.synctex.gz` or `.synctex` sidecar output into `SyncTexArtifact` when present.
  - Has a test that compiles a simple LaTeX document when `xelatex` is available.
  - Has an optional smoke test for the original TeXpresso `test/simple.tex` fixture when the original repo can be located.
  - Has optional original fixture smoke tests for `include.tex`, `missing-input.tex`, and `includegraphics.tex` when `xelatex` and the original source tree are available.

- `oxipresso-platform`
  - Adds target-specific modules: `windows.rs`, `linux.rs`, `macos.rs`.
  - Provides `current_platform`, `canonicalize_for_display`, and `font_directories`.
  - Defines `FileWatcher`, `FileWatchEvent`, and `PollingFileWatcher`.
  - Exposes `file_watcher()` through platform-specific modules for Windows, Linux, macOS, and fallback targets.
  - Current watcher implementation is portable polling and only marks a change clean after the consumer accepts it, preserving retry behavior for half-written artifacts.

- `oxipresso-render`
  - Adds `RenderBackend` trait.
  - Adds `StubRenderBackend`.
  - Adds `AutoRenderBackend`.
  - Adds `PdfMetadataRenderBackend`.
  - Validates PDF artifacts by checking for a `%PDF-` header.
  - Counts visible PDF page objects by scanning `/Type /Page` while avoiding `/Type /Pages`.
  - Walks DVI/XDV opcode streams far enough to count `bop` page-begin opcodes and avoids counting `bop` bytes inside skipped payloads.
  - Parses a first DVI/XDV display-list subset for `set_rule` and `put_rule`.
  - Parses basic glyph opcodes: `set_char`, `set1..4`, and `put1..4`.
  - Parses DVI font definitions and font selection opcodes enough to associate glyph placeholders with the current font.
  - Sizes glyph placeholders from font scaled size when available, with fixed fallback dimensions when font data is absent.
  - Tracks common DVI movement and stack opcodes while collecting rule rectangles.
  - Renders parsed rules as dark rectangles and glyphs as placeholder marks on the placeholder page, giving XDV/DVI artifacts their first visible nonblank output path.
  - Falls back to one placeholder page for valid PDFs whose page objects are not visible, such as future compressed-object PDFs.
  - Returns a stable placeholder RGBA page when no real renderer is available.
  - Adds optional `pdfium` feature backed by `pdfium-auto 0.3.0` and `pdfium-render 0.8.37`.
  - Adds optional `pdfium-bundled` feature for future bundled PDFium distribution.
  - `PdfiumRenderBackend` loads PDF artifacts from bytes and renders real RGBA pages.
  - `AutoRenderBackend` prefers `PdfiumRenderBackend` for PDFs when the `pdfium` feature is enabled and falls back to `PdfMetadataRenderBackend` on failure.
  - PDFium binding is cached per backend instance, avoiding repeated binding attempts inside a long-running viewer.
  - Adds an optional `freetype` feature (vcpkg freetype on Windows, pkg-config `freetype2` on Linux; default builds stay dependency-free).
  - Adds `xdv` module: a complete XDV/DVI stream parser following the TeXpresso engine's exact output layout — classic DVI opcodes, XDV native font defs (opcode 252: font_id[4], 16.16 size, flags, name, face index, optional color/extend/slant/embolden), `SET_GLYPHS` (253) and `SET_TEXT_AND_GLYPHS` (254) glyph arrays with fixed-point offsets, `pdf:pagesize` specials for page dimensions, plus a minimal TFM width parser for classic set_char advances.
  - Adds `ft` module: minimal hand-written FreeType FFI whose glyph-slot access is layout-independent — the slot is located by scanning the face for a back-referencing pointer (`slot->face == face`) with a heap-cluster filter, and the rendered `FT_Bitmap` is located by signature (rows/width/pitch/buffer/gray pixel mode), surviving FreeType 2.14 struct reshuffles.
  - Adds `XdvGlyphRenderBackend`: resolves font files through a `FontResolver` (native fonts try `.otf`/`.ttf`, classic try `.pfb`/`.ttf`/`.otf`; face index from the font def; Type1 char codes fall back to direct glyph indices), rasterizes glyphs with FreeType at 96 dpi, caches glyphs per (font, face, code, size), and composites glyphs (with FreeType bearing) plus rules onto RGBA pages.
  - `AutoRenderBackend::with_xdv_glyph_backend` attaches the glyph renderer for XDV/DVI artifacts; the CLI installs it behind the `freetype` feature with a kpsewhich-based resolver.
  - Has tests for synthetic XDV parsing (pages, page dims, fonts, glyph runs, rules), synthetic TFM widths, a system-font FreeType rasterization smoke (Windows `times.ttf`), and an env-gated real-XDV render smoke (`OXIPRESSO_XDV_SMOKE`). **`char_width_dvi`/`parse_tfm_widths` validated correct against an authoritative reference (round 90):** the classic (non-native) per-code advance at 10pt for real `cmr10.tfm` matches `tftopl`'s independent widths across all 128 codes (≤1 unit = fix-word rounding), confirming the `(tfm_w * size_pt*2^20) >> 24` formula and that TFM fixes are already design-size-normalized (so `DESIGN_SIZE` correctly cancels). This spacing is invisible to the dark-pixel smoke, so it is now pinned by the golden test `classic_tfm_advances_match_tftopl_reference` (self-skips without a TeX distribution).

- `oxipresso-synctex`
  - Decodes plain and gzip-compressed SyncTeX bytes.
  - Parses basic `Output:` and `Input:<index>:<path>` metadata, plus the `Magnification`/`Unit`/`X Offset`/`Y Offset` header fields into `SyncTexDocument` (spec defaults 1000/1/0/0 when absent; tested by `parses_synctex_header_fields`). Confirmed against the engine's writer (`xetex-synctex.c` records emit `curh / unit`, and `curh`/`curv` are already magnification-baked physical sp): the client inverse is `pt = stored × unit / 65536`, so the CLI's `pt ↔ ×65536` (unit=1) conversion is exactly right. `unit` defaults to 1 for every TeX/XeTeX producer (the 8192 "shorter records" knob is never enabled by default), so no unit scaling is needed for realistic sidecars; `Magnification` is informational only (for logical-coordinate clients), which is why it is deliberately not applied to physical-coordinate conversion.
  - Parses sheet/page starts and common node records such as `[`, `(`, `h`, `g`, `k`, preserving page, input index, source line, and coordinates.
  - Provides lookup by input index.
  - Provides forward lookup by input index or source path, returning the last box record at-or-before the requested source line (earliest as fallback) — the de-facto forward-sync behavior, corrected from an earlier nearest-by-absolute-distance heuristic that could jump to a later line's box.
  - Normalizes SyncTeX/editor paths for forward lookup by converting backslashes, collapsing duplicate slashes, removing leading `./`, and matching absolute/relative suffixes in either direction.
  - Provides reverse lookup by page and point, preferring the record whose horizontal box span `[x, x+width]` contains the click (measuring distance to the nearest edge of the span) and falling back to squared corner distance when width is unknown; among equidistant candidates it resolves to the innermost (narrowest known width) so a click inside nested boxes lands on the specific record, not its enclosing line box. Per the engine's syncTeX writer (`xetex-synctex.c`), stored coordinates are in `unit`·sp (our output uses `Unit:1` ⇒ sp) and the **origin is the top-left corner of the page with y increasing downward (PDF mode)** — so the incoming click point must be expressed top-left/y-down to match. Reverse currently uses symmetric squared corner distance on the vertical axis (correct for candidate ranking without modelling the height/depth box extent in the y-down space); vertical box containment remains unimplemented but its orientation is now pinned.
  - Has tests for plain metadata, compressed artifact parsing, record parsing, forward lookup, Windows/relative path matching, reverse lookup, and invalid input index handling.
  - Is intentionally not a full TeXpresso-compatible SyncTeX coordinate mapper yet.

- `oxipresso-viewer`
  - Adds `ViewerState`.
  - Adds page navigation and simple viewer toggles.
  - Adds `load_artifact_with_renderer` so viewer state can update page count from any `RenderBackend`.
  - Clamps the current page when a new artifact has fewer pages than the previous one.
  - Adds optional `gui` feature using `eframe 0.34.3` with `wgpu`.
  - Adds `ViewerGuiApp` and `run_native_viewer`.
  - GUI can display the current rendered page as an egui texture. With `pdfium`, PDF pages are real raster output; without it, they are placeholders.
  - GUI supports page navigation, zoom, fit page/fit width, crop toggle, invert toggle, and theme toggle state.
  - `ViewerState` stores an optional SyncTeX hit position.
  - GUI draws a lightweight marker for the current page's SyncTeX hit using an approximate TeX-point-to-page mapping and asks the scroll area to center it.
  - GUI keeps the previous texture if a new render fails, which preserves the no-flicker behavior needed for live updates later.
  - Adds `load_artifact_from_path` and `artifact_kind_from_path_and_bytes` helpers shared by GUI and CLI.
  - GUI supports watching an artifact path through `oxipresso-platform` and reloading when the file appears or changes.
  - Polling reloads do not advance the watched timestamp on failed artifact validation, so half-written PDFs are retried.

- `oxipresso-testkit`
  - Adds shared test fixtures and S-expression parse helper.
  - Adds optional original TeXpresso fixture lookup:
    - first checks `TEXPRESSO_SRC`;
    - then searches ancestor sibling directories such as `texpresso-src`, `texpresso`, and `let-def/texpresso`.
  - Fixture-dependent tests skip automatically when the original repo is unavailable.

- `oxipresso-cli`
  - Adds `oxipresso` binary.
  - Adds optional `oxipresso-viewer` binary behind the `gui` feature.
  - Parses TeXpresso-compatible flags: `-I`, `-json`, `-lines`, `-texlive`, `-tectonic`, `-test-initialize`, `-stream`.
  - `-tectonic` is rejected with a clear "not implemented yet" error rather than silently falling back to TeX Live/kpsewhich resolution (the Tectonic provider is unimplemented; `OxipressoApp` always uses the kpsewhich resolver when available, so the flag must not imply otherwise). `-texlive` is accepted and selects `PackageProvider::Texlive`.
  - Initializes `XetexEngine`.
  - Reads editor commands from stdin and applies VFS updates.
  - Tolerates malformed/unknown editor commands like the original (TeXpresso `src/frontend/editor.c` prints `[command] …` to stderr and continues): `handle_editor_line` reports a parse error on stderr and skips the line rather than aborting the whole session, so a single bad command cannot kill the live preview. Only genuine engine/VFS faults remain fatal. Tested by `unknown_or_malformed_editor_command_is_skipped_not_fatal`.
  - Emits lookup messages by draining VFS lookup events.
  - `register` no longer emits `lookup-file promised` immediately; the promised message is emitted when the engine actually requests the registered file.
  - Supports `OXIPRESSO_ENGINE=external` to select the external `xelatex` backend.
  - Supports `OXIPRESSO_ARTIFACT_OUT=<path>` to write the current engine artifact to disk.
  - Supports `OXIPRESSO_SYNCTEX_OUT=<path>` to write the current engine SyncTeX artifact to disk.
  - Converts engine diagnostics into TeXpresso-style `truncate`/`append`/`flush` editor messages.
  - In `-lines` mode, converts diagnostics into `truncate-lines`/`append-lines`/`flush` editor messages; streamed `out`/`log` channels also honor `-lines` (complete lines only, trailing partial lines withheld) via `stream_messages_from_events`.
  - Engine initialization/rebuild errors with diagnostics are now reported to the editor without failing the CLI session.
  - In non-stream mode, primes the root document into VFS under the root file name before engine initialization.
  - Maintains an internal `ViewerState`.
  - Refreshes the internal viewer state from engine artifacts after initialization/resume using `PdfMetadataRenderBackend`.
  - Performs first-stage live rebuilds: editor changes and `rescan` compute/apply a restart policy, then restart or full initialize the selected engine when not paused.
  - The current FFI and external backends still use full restart behavior, but the CLI rebuild loop now follows the `RestartPolicy` boundary.
  - Emits `input-file` messages for successfully opened inputs, including root reads during initialization.
  - Handles simple viewer commands for previous/next page and crop/invert/theme state in the internal viewer model. `Crop`/`Invert` call real `ViewerState` toggles; `Theme { bg, fg }` (round 99) now applies the editor's requested colors faithfully to TeXpresso `main.c`: floats are converted `clamp(0..1)*255` → RGB and sent to `AutoRenderBackend::set_theme`, which sets the glyph renderer's page background (canvas fill) and default ink (rules + uncolored glyphs), clearing the page cache so rendered pages repaint. (The GUI checkbox on `ViewerState::themed` remains a separate manual toggle.) Tested by `theme_command_sets_themed_and_applies_colors` (CLI) and `theme_colors_paint_background_and_ink` (freetype render).
  - Handles `synctex-forward` by using the latest parsed SyncTeX document to update internal viewer page state and store hit coordinates.
  - `synctex-forward` integration is tested with editor path variants matching Windows-style absolute SyncTeX input paths.
  - Registers the root directory and `-I` include paths as VFS disk roots for all engine backends.
  - Installs `KpsewhichResolver` on the VFS when kpsewhich is detectable (`OXIPRESSO_TEXLIVE=0` disables it), so the real engine can pull format sources, classes, and fonts from the TeX distribution.
  - Has a Rust migration of the original stream-mode flow: `register`, `open`, then `resume` reads the root from editor VFS content and emits successful lookup/input messages.
  - Has a Rust test for the original register/deferred lookup workflow: stream mode can register a missing include before resume, the engine observes a promised lookup, and opening the promised file triggers a rebuild that observes the include as successful input.
  - Has a Rust test for the original non-blocking lookup workflow: an engine lookup can fail for a missing file, the editor can later `open` that file, and the CLI performs a full rebuild that observes the file as a successful input.
  - Has an end-to-end JSON protocol smoke test for initialization messages emitted by `run_with_io`.
  - `oxipresso-viewer` can open an existing `.pdf`, `.xdv`, or `.dvi` artifact path and launch the GUI viewer.
  - `oxipresso-viewer --watch <artifact>` launches the GUI and polls the artifact path for changes.
  - This enables a temporary live-preview workflow with `OXIPRESSO_ARTIFACT_OUT=<same artifact path>`.

## Verified

Test totals currently: **120 passing in the default stub workspace** across 11 crates (cli 33, render 23 default, vfs 18, engine-xetex 11, engine-external 8, synctex 11, viewer 4 default, editor-protocol 8, platform 2, testkit + engine-api), plus **39/39** with the `freetype` render feature (adds image-decode, premultiplied box-filter compositing, per-axis matrix scale, the image-edit-invalidation regression, the classic-TFM-advance golden test validated against `tftopl`, golden tests for the extend/embolden/slant bitmap transforms including the positive-and-negative-slant overhang fix, `blit_gray` coverage/clipping, and editor-theme background/ink painting), **8/8** viewer GUI, and env-gated real-engine tests (`real_xetex_bootstrap...` incl. include/includegraphics/missing-input/reverse-syncTeX/restart-policy, and `real_engine_protocol_snapshot` reverse-over-wire). CLI tests that mutate the process-global `OXIPRESSO_ENGINE` are serialized on a shared lock so parallel `cargo test` is deterministic (verified green over repeated full-workspace runs). The suite also passes in the **release profile** (`cargo test --workspace --release`: 0 failed, 107 passed — round 79), so there is no debug/release-only divergence (no reliance on overflow checks or `debug_assert!`).

**Test-mode contract:** the default `cargo test --workspace` builds the **stub** engine (no `OXIPRESSO_USE_REAL_XETEX`), and that is the supported/verified invocation. A few CLI tests (e.g. the `run_with_io` stream/json tests) assert the *stub* engine's fixed output and are NOT guarded by `real_mode()`; running the whole workspace as a real-engine build (setting `OXIPRESSO_USE_REAL_XETEX=1` at build time) makes ~3 of them fail spuriously — that is a harness mis-invocation, not a regression. Real-engine behavior is covered by the dedicated env-gated tests (`real_xetex_bootstrap...`, `real_engine_protocol_snapshot`), which self-skip unless real mode is active.

Earlier snapshot (all modes, round 12):

- Stub mode: 73 tests green across 9 crates (cli 24, render 17, vfs 10, engine-external 8, viewer 3, synctex 6, editor-protocol 3, platform 2, engine-api 0).
- Freetype mode: 20/20 render tests (including real-XDV smoke and system-font rasterization).
- GUI feature: compiles for cli gui, cli gui+freetype, cli gui+pdfium.
- Real engine mode: 5/5 engine-xetex tests (bootstrap + typeset + SyncTeX + output events).
- Real-XDV render smoke: 2/2 (parse + glyph render with page digest).
- Real protocol snapshot: 1/1 (init + rebuild message sequences).
- Reverse SyncTeX: 1/1 (real sidecar → source location).
- Formatting: `cargo fmt --all --check` passes.

Additional opt-in real XeTeX build diagnostic:

```powershell
$env:OXIPRESSO_USE_REAL_XETEX='1'
$env:TEXPRESSO_SRC='F:\code\texpresso-src'
$env:VCPKG_ROOT='F:\code\vcpkg'
cargo check -p oxipresso-engine-xetex-sys
cargo test -p oxipresso-engine-xetex --no-run
```

Result (current machine state):

- The build script enters real mode, finds `F:\code\texpresso-src`, and discovers every vcpkg dependency from `F:\code\vcpkg` (triplet `x64-windows-static-md`).
- The full real engine (engine C, engine/layout C++, dpx C, real shim C) compiles cleanly with warnings disabled.
- `cargo test -p oxipresso-engine-xetex --no-run` LINKS the whole real engine into the Rust test binary; the only extra link input needed was `advapi32.lib` for ICU's Windows time-zone detection, now emitted by `build.rs`.
- Running the real-mode test binary confirms `oxipresso_xetex_run` reaches the real engine and returns control cleanly through the shim's `setjmp`/`longjmp` abort capture when no format file is available (no crash, no hang).
- In real mode the two stub-behavior tests (`ffi_stub_initializes`, `ffi_stub_uses_engine_io_callbacks`) now self-skip via `XetexEngine::real_mode()` instead of failing; the file-kind mapping test and the opt-in smoke test pass.
- vcpkg environment notes for this machine: `VCPKG_ROOT=F:\code\vcpkg` must be set for real-mode builds; the vcpkg tree needed manual source downloads for `meson`, `harfbuzz`, and `icu` tarballs into `F:\code\vcpkg\downloads` because vcpkg's bundled curl fails TLS through the local proxy (SOCKS5 `127.0.0.1:10808`); direct `curl.exe` to `codeload.github.com` and `github.com` release assets works and cached tarballs were hash-verified.
- This does not affect default stub builds, which remain dependency-free.

Historical note: before the vcpkg setup, real mode failed early with a `freetype2` vcpkg discovery error; that blocker is now resolved.

Additional real XeTeX format bootstrap + typesetting verified (the defining milestone):

```powershell
$env:OXIPRESSO_USE_REAL_XETEX='1'; $env:TEXPRESSO_SRC='F:\code\texpresso-src'; $env:VCPKG_ROOT='F:\code\vcpkg'
cargo test -p oxipresso-engine-xetex -- --nocapture --test-threads=1
cargo run -q -p oxipresso-cli --bin oxipresso -- -test-initialize <temp>\oxi-real-smoke.tex
```
Result:

- `real_xetex_bootstrap_builds_format_and_typesets_simple` passes: with `KpsewhichResolver` installed, the first `initialize` bootstraps the format from `xelatex.ini` (spotless), persists `texpresso.fmt` to `OXIPRESSO_XETEX_FORMAT`, typesets `test/simple.tex`, and returns a nonempty real XDV `DocumentArtifact`; a second `initialize` reuses the persisted format and produces an artifact again.
- All 5 tests in the crate pass in real mode; stub-behavior tests self-skip via `XetexEngine::real_mode()`.
- CLI end-to-end: `OXIPRESSO_ARTIFACT_OUT` captured a real 1011-byte XDV (first bytes `F7 07` XDV magic) and `texpresso.fmt` was 22.3 MB; stdout showed the full lookup flow with extension guessing feeding kpsewhich (`lookup-file read failed "cmmi6"` -> `read successful "cmmi6.tfm"`), the XDV write, and the `.aux` read.
- Known runtime noise: fontconfig prints `Cannot load default config file` (non-fatal; see Not Done Yet).

Additional protocol snapshots verified (real engine, gated CLI test `real_engine_protocol_snapshot`):

- Initialization sequence: `(truncate out 0)` + `(truncate log 0)` first, then the engine's stdout/log appends — including the XeTeX banner and TeX's per-character file-open echo (`(`, `m`, `a`, `i`...) — then `(flush)`, `input-file`, and a successful `lookup-file` for the root.
- The SyncTeX document is parsed and attached after initialization.
- Change-rebuild sequence: both channels re-truncate, the file open is re-echoed (`(main.tex`), and a flush closes the run.
- Wire formats verified: `(truncate out 0)` in S-expression and `["truncate","log",0]` in JSON.

Additional XDV real-glyph rendering verified (freetype feature):

```powershell
$env:VCPKG_ROOT='F:\code\vcpkg'; $env:OXIPRESSO_XDV_SMOKE='F:\code\oxipresso\target\simple-real.xdv'
cargo test -p oxipresso-render --features freetype
cargo check -p oxipresso-cli --features freetype
```

Result:

- 17/17 render tests pass with the `freetype` feature (15 without it, keeping default builds dependency-free).
- `freetype_rasterizes_glyph_from_system_font` rasterizes a real glyph from `C:\Windows\Fonts\times.ttf` (12x11 bitmap, top bearing 11) through a synthetic XDV.
- `xdv_glyph_render_smoke_renders_real_xdv` renders page 1 of the real engine's `simple-real.xdv` end to end — native LM OTF fonts + classic Type1 math fonts resolved through kpsewhich — and asserts substantial dark-pixel coverage.
- FreeType is 2.14.3 on this machine, whose `FT_FaceRec`/`FT_GlyphSlotRec` layouts differ from the hand-written assumptions; the bindings therefore locate the glyph slot by back-reference scan and the bitmap by field-signature scan, both validated at runtime. `FT_LOAD_NO_BITMAP|FT_LOAD_NO_HINTING` crashed this build; `FT_LOAD_DEFAULT` is used instead.

Additional PDFium smoke verified on Windows:

```powershell
$env:OXIPRESSO_PDFIUM_SMOKE_PDF='<temp>\main.pdf'
cargo test -p oxipresso-render --features pdfium pdfium_smoke_renders_real_pdf_when_requested
```

Result:

- generated a real PDF with `xelatex`;
- `PdfiumRenderBackend` rendered page 0;
- output dimensions were larger than the placeholder dimensions;
- RGBA buffer size matched `width * height * 4`.

Additional smoke verified:

```powershell
$env:OXIPRESSO_ENGINE='external'
$env:OXIPRESSO_ARTIFACT_OUT='<temp>\out.pdf'
cargo run -q -p oxipresso-cli -- -test-initialize <temp>\main.tex
```

Result:

- command exited successfully;
- `out.pdf` existed and had nonzero size;
- stdout was emitted as `(truncate out 0)`, `(append out "...")`, `(flush)`;
- root lookup emitted `(lookup-file read successful "main.tex")`.

Additional external compile-failure path verified (real `xelatex`, round 56):

```powershell
$env:OXIPRESSO_ENGINE='external'
cargo run -q -p oxipresso-cli --bin oxipresso -- -test-initialize <temp>\broken.tex   # \undefinedmacroxyz
```

Result: exit **0** (the CLI session survives a failed compile rather than crashing), and the editor wire carried `(append out "…")` including TeX's actual error text (`broken.tex:3: Undefined control sequence.` and the `l.3 …` context line). This is the external backend reading the `.log` and emitting an `Error` diagnostic, confirming the "report engine errors to the editor without failing the session" invariant for a real syntax error.

Additional shipped-binary runtime verification (stub `target/debug/oxipresso.exe`, rounds 69/75/76): the compiled `oxipresso.exe` binary runs and emits the correct protocol on the real stdin/stdout wire — sexp mode yields `(truncate out 0)`/`(append out "Oxipresso XeTeX FFI stub\n")`/`(input-file 0 "main.tex")`/`(lookup-file read successful "main.tex")` (proving the FFI-callback→mirror→info-buffer chain reaches the wire), and `-json` mode yields the JSON-array equivalents `["truncate","out",0]` / `["append","out","…"]` / `["input-file",0,"main.tex"]` (proving the shipped exe honors both protocol formats, not just in-process `run_with_io`). Exit 0 and a persisted artifact in both cases.

## Not Done Yet

- Incremental rebuild status: three of four layers are done — (1) format cache (no re-bootstrap), (2) renderer-side page digest cache (unchanged XDV pages reuse rendered output), (3) `read_files`-based rebuild skipping (edits to files the engine never read cost nothing), (4) kpsewhich persistent resolution cache (warm rebuilds 15.5s → ~600ms). The remaining layer — resuming typesetting from an engine-state checkpoint instead of a full re-run — is the only open piece. The original uses fork() at read "fences" (copy-on-write process snapshots; see `src/frontend/engine_tex.c` fences and `engine/main/fork.c`), which has no Windows equivalent; per the design constraints this must become one cross-platform engine-state serialization/restore model, a large self-contained project (the engine globals span pool/equiv/trie/font memory structures). At ~600ms warm rebuilds the preview is already interactive; the serialization layer would shave the remaining ~300-400ms of format-load + input-replay time.
- The real shim does not implement `synctex_texpresso_extension` yet, and the shim reports `mtime 0` for inputs. The plain `.synctex` sidecar is captured and parsed (see Verified).
- The FFI stub still does not parse TeX (by design); the default build keeps the stub.
- CLI now emits basic `truncate`/`append`/`flush` messages from engine diagnostics, but does not yet mirror every TeXpresso input-file/indexing nuance from the original engine.
- CLI can persist SyncTeX sidecar bytes and `oxipresso-synctex` can parse input/output/page/node metadata plus first forward/reverse lookup helpers. CLI acts on `synctex-forward` at the page/marker level, and reverse SyncTeX resolves viewer clicks to source locations. The full round trip is now validated against **real engine output**: `real_engine_protocol_snapshot` (gated) asserts `OxipressoApp::synctex_reverse_message` maps a page-1 point to a source location through the real `.synctex` sidecar and emits the editor-wire `synctex` notification; `real_xetex_bootstrap...` asserts `reverse_search_page_point` on the real sidecar. What remains is precise media-box scrolling in the egui view and the standalone `oxipresso-viewer` binary (no editor wire) still surfacing reverse hits as status + marker only.
- CLI can write the produced PDF artifact to disk through `OXIPRESSO_ARTIFACT_OUT` and now updates internal `ViewerState`; a separate `oxipresso-viewer --watch <artifact>` process can watch that artifact path through the platform watcher abstraction.
- `-gui` closes the integration gap: engine, window, and editor wire share one process with direct artifact-to-window refresh. The remaining live-preview gap is only in the standalone `oxipresso-viewer --watch` binary (it still reloads through the artifact file + polling watcher).
- `oxipresso-render` can render real PDF pages only when the optional `pdfium` feature is enabled; default builds still use placeholder fallback.
- XDV/DVI real glyph rendering works behind the `freetype` feature (native OTF + classic Type1 via kpsewhich) with color specials and the slant/extend/embolden transform trio. **Images now render**: the parser recognizes `pdf:image` specials in both engine forms (bbox from `\includegraphics`, matrix from `\XeTeXpicfile`), producing `XdvElement::Image` entries included in page digests; the glyph backend decodes PNGs through the pure-Rust `png` crate (behind the `freetype` feature) and composites them onto the page with premultiplied-alpha box-filter scaling (nearest for 1:1/upscale, correct area averaging on downscale so transparent pixels never darken the result), with an `ImageLoader` trait — the CLI installs one rooted at the document directory + include paths. Verified end to end with the original TeXpresso `includegraphics.tex` fixture through the real engine (image resolved via VFS, XDV produced, exit 0). bbox aspect now honors width-only, height-only, both, and neither, and the matrix form keeps the x (`a`) and y (`d`) scales separate so non-uniform `\XeTeXpicfile` is no longer distorted (tested by `parses_nonuniform_matrix_image_xy_scale`). **Image-edit invalidation (fixed):** the renderer reads each referenced image's current bytes via the loader on every page render, memoizes decoded images by `(path, content-hash)`, and folds the images' content hashes into the page-cache key — so editing an image file in place (same path/geometry, hence identical XDV) still invalidates the cached rendered page instead of showing a stale image. Regression-tested by `in_place_image_edit_invalidates_cached_page` (same XDV, swapped bytes ⇒ red→blue re-render). Still TODO: sub-pixel positioning, and wiring the glyph backend into the egui viewer's zoom/crop path beyond full-page rendering.
- The PDF page counter is intentionally lightweight and may not see page objects hidden in compressed object streams; it currently falls back to one page for a valid PDF in that case.
- No native platform file watcher is wired yet; current GUI watch mode is portable polling behind the `oxipresso-platform` `Box<dyn FileWatcher>` abstraction. A native Windows `ReadDirectoryChangesW` watcher was prototyped and reverted: the key Win32 gotchas (recorded so a retry doesn't rediscover them) are (1) an overlapped `ReadDirectoryChangesW` returns FALSE with `GetLastError()==ERROR_IO_PENDING` as the *normal* queued-completion signal — treating any FALSE as fatal kills the worker immediately so no notifications ever arrive; (2) cancelling a pending synchronous read by closing its handle from another thread is a known deadlock — the correct pattern is an overlapped read on an event plus `CancelIoEx` and only `CloseHandle` after `join`; (3) the watcher's `Drop` runs during test panic-unwind, so a hung `join` there masquerades as an infinite test rather than an assertion failure.
- No Tectonic provider is implemented yet; the TeX Live path is covered by `KpsewhichResolver` (kpsewhich with persistent cache), but not the full original texlive dependency-tape validation.
- Linux build verification: the pure-Rust crates (`oxipresso-platform`, `oxipresso-render`, `oxipresso-vfs`, `oxipresso-engine-api`, `oxipresso-editor-protocol`, `oxipresso-synctex`, `oxipresso-testkit`, `oxipresso-engine-external`) not only cross-check but **fully build** for BOTH Linux ABIs — `cargo build --target x86_64-unknown-linux-musl --lib` and `cargo build --target x86_64-unknown-linux-gnu --lib` for all eight both succeed (full LLVM codegen/monomorphization to Linux rlibs, a strictly stronger signal than `cargo check --target`, which only type-checks). This confirms the platform-neutrality design constraint holds down to codegen for both `*-linux-musl` and `*-linux-gnu`. What is NOT achievable from this Windows host: linking Linux **binaries/test executables** (needs a musl/gnu cross linker — `cc` for the target isn't present) and building the C-engine crates (`oxipresso-engine-xetex-sys`/`-xetex`/`-cli`, whose `build.rs` invokes `cc-rs`, which has no Linux C toolchain here); and running `cargo test` on Linux (needs a Linux runner). This was probed directly this session (round 68): WSL Debian is present but has no `cargo`/`rustc` installed, no outbound network (NAT/DNS + proxy failure, no `curl`), and `apt` needs a sudo password — so installing a Linux toolchain via rustup/apt is not possible here either; the `x86_64-unknown-linux-gnu` **std** was added on the Windows side (for `--target` lib builds) but that does not enable running Linux tests. Full Linux build/test therefore genuinely requires a separate environment. No CI has been wired.
- No macOS implementation or verification has been done.
- Original TeXpresso fixture coverage now includes `simple.tex`, `include.tex`, `missing-input.tex`, and `includegraphics.tex` through the external backend when fixtures and `xelatex` are available. **`include.tex` (with `\input{test.tex}` resolved through an include path) and `includegraphics.tex` (PNG resolved through the VFS, `pdf:image` special present in the XDV) now also run through the REAL FFI engine** via the extended gated test `real_xetex_bootstrap_builds_format_and_typesets_simple` (runs only in real mode, ~35s). The core behaviors from `test_stream.sh`, `test-register.sh`, and the non-blocking missing-file path in `test-lookup-file.sh` have Rust unit coverage, and the real-engine init/rebuild protocol flow has a gated snapshot test (`real_engine_protocol_snapshot`).

## Important Design Constraints

- Keep core Rust crates platform-neutral.
- Put OS-specific code only in `oxipresso-platform`.
- Do not introduce Unix-only `fork`, `socketpair`, or `SCM_RIGHTS` into the Rust engine path.
- Do not make Linux a special fork-based backend; eventual incrementality should use one checkpoint/restart model across Windows/Linux/macOS.
- Keep `TypesettingEngine` and `EngineIo` stable so the FFI backend can later be replaced by a pure Rust backend.
- Treat the current C shim as a temporary adapter point, not product logic.

## Suggested Next Steps

1. Implement the TeXpresso-defining incremental checkpoint/restart model so editor changes stop requiring a full engine restart.
2. Polish XDV glyph rendering: XDV specials, `pic_file` images, color/extend/slant/embolden transforms, and decide whether Windows builds should enable `freetype` by default.
3. Replace the portable polling watcher with native Windows/Linux watchers where useful, or add a proper live CLI/engine-to-viewer event path.
4. Expand original TeXpresso fixture integration tests:
   - async register lookup and lookup-file restart scenarios through the real engine.
   - fixture runs (`include.tex`, `includegraphics.tex`) through the FFI XeTeX backend.
5. Verify Linux builds of the real mode via pkg-config, and keep macOS as the later placeholder.

## Current Git State Expectation

Expected untracked project files:

- `.gitignore`
- `Cargo.lock`
- `Cargo.toml`
- `README.md`
- `AGENTS.md`
- `crates/`

`target/` should remain ignored.
