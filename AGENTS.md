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
- CLI can persist the current engine artifact when `OXIPRESSO_ARTIFACT_OUT` is set. With the external backend, this writes a real PDF.
- Produced engine artifacts are now also passed through an internal viewer metadata pipeline.
- An optional `eframe/egui` + `wgpu` GUI shell now exists for opening and polling artifact files.
- GUI artifact watching now goes through an `oxipresso-platform` file-watcher abstraction. The current backend is portable polling, and native Windows/Linux/macOS watchers can replace it later without changing viewer code.
- PDF rendering can use real PDFium rasterization when the optional `pdfium` feature is enabled; otherwise it falls back to placeholder pages.
- The external backend captures SyncTeX sidecar output. `oxipresso-synctex` decodes gzip/plain SyncTeX, parses `Output:` / `Input:` metadata plus common page/node records, and can perform first nearest-line forward and nearest-point reverse lookups. CLI `synctex-forward` now updates the internal viewer page and stores hit coordinates; the optional GUI can draw a lightweight marker and request scrolling to it, but full coordinate behavior is still pending.

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
  - Adds default `TypesettingEngine::output_synctex()` for optional SyncTeX sidecar artifacts.

- `oxipresso-editor-protocol`
  - Parses S-expression editor commands.
  - Parses JSON editor commands.
  - Serializes editor messages as S-expression or JSON.
  - Supports first-stage commands including `open`, `open-base64`, `close`, `change`, `change-lines`, `change-range`, `register`, `pause`, `resume`, page/window/theme commands, and SyncTeX forward command.

- `oxipresso-vfs`
  - Implements editor-backed virtual file system.
  - Supports path normalization using forward slashes.
  - Supports `open`, `open-base64`, `close`, `change`, `change-lines`, `change-range`.
  - Implements UTF-16 code unit to UTF-8 byte offset conversion for `change-range`.
  - Tracks promised files.
  - Opening a promised file through the editor now reports a change at offset 0 so promised-file fulfillment can trigger a rebuild.
  - Tracks files that were requested by engine reads, including failed lookup attempts.
  - Opening a file after a failed engine lookup now reports a change at offset 0 so non-blocking `lookup-file failed` workflows can trigger a rebuild when the editor provides the file.
  - Supports disk-root fallback search after editor/disk cached content. CLI seeds disk roots from the root directory plus `-I` include paths.
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

- `oxipresso-engine-xetex`
  - Adds safe Rust wrapper `XetexEngine`.
  - Implements `TypesettingEngine`.
  - Currently calls the FFI stub and reports engine name `xetex-ffi-stub`.
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
  - `apply_change_hint` currently returns `FullRestartRequired`.

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

- `oxipresso-synctex`
  - Decodes plain and gzip-compressed SyncTeX bytes.
  - Parses basic `Output:` and `Input:<index>:<path>` metadata.
  - Parses sheet/page starts and common node records such as `[`, `(`, `h`, `g`, `k`, preserving page, input index, source line, and coordinates.
  - Provides lookup by input index.
  - Provides first forward lookup by input index or source path, returning the nearest source-line hit.
  - Normalizes SyncTeX/editor paths for forward lookup by converting backslashes, collapsing duplicate slashes, removing leading `./`, and matching absolute/relative suffixes in either direction.
  - Provides first reverse lookup by page and point, returning the nearest source record.
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
  - Rejects mutually exclusive `-texlive` and `-tectonic`.
  - Initializes `XetexEngine`.
  - Reads editor commands from stdin and applies VFS updates.
  - Emits lookup messages by draining VFS lookup events.
  - `register` no longer emits `lookup-file promised` immediately; the promised message is emitted when the engine actually requests the registered file.
  - Supports `OXIPRESSO_ENGINE=external` to select the external `xelatex` backend.
  - Supports `OXIPRESSO_ARTIFACT_OUT=<path>` to write the current engine artifact to disk.
  - Supports `OXIPRESSO_SYNCTEX_OUT=<path>` to write the current engine SyncTeX artifact to disk.
  - Converts engine diagnostics into TeXpresso-style `truncate`/`append`/`flush` editor messages.
  - In `-lines` mode, converts diagnostics into `truncate-lines`/`append-lines`/`flush` editor messages.
  - Engine initialization/rebuild errors with diagnostics are now reported to the editor without failing the CLI session.
  - In non-stream mode, primes the root document into VFS under the root file name before engine initialization.
  - Maintains an internal `ViewerState`.
  - Refreshes the internal viewer state from engine artifacts after initialization/resume using `PdfMetadataRenderBackend`.
  - Performs first-stage live rebuilds: editor changes and `rescan` compute/apply a restart policy, then restart or full initialize the selected engine when not paused.
  - The current FFI and external backends still use full restart behavior, but the CLI rebuild loop now follows the `RestartPolicy` boundary.
  - Emits `input-file` messages for successfully opened inputs, including root reads during initialization.
  - Handles simple viewer commands for previous/next page and crop/invert/theme state in the internal viewer model.
  - Handles `synctex-forward` by using the latest parsed SyncTeX document to update internal viewer page state and store hit coordinates.
  - `synctex-forward` integration is tested with editor path variants matching Windows-style absolute SyncTeX input paths.
  - Registers the root directory and `-I` include paths as VFS disk roots for all engine backends.
  - Has a Rust migration of the original stream-mode flow: `register`, `open`, then `resume` reads the root from editor VFS content and emits successful lookup/input messages.
  - Has a Rust test for the original register/deferred lookup workflow: stream mode can register a missing include before resume, the engine observes a promised lookup, and opening the promised file triggers a rebuild that observes the include as successful input.
  - Has a Rust test for the original non-blocking lookup workflow: an engine lookup can fail for a missing file, the editor can later `open` that file, and the CLI performs a full rebuild that observes the file as a successful input.
  - Has an end-to-end JSON protocol smoke test for initialization messages emitted by `run_with_io`.
  - `oxipresso-viewer` can open an existing `.pdf`, `.xdv`, or `.dvi` artifact path and launch the GUI viewer.
  - `oxipresso-viewer --watch <artifact>` launches the GUI and polls the artifact path for changes.
  - This enables a temporary live-preview workflow with `OXIPRESSO_ARTIFACT_OUT=<same artifact path>`.

## Verified

Last verified from:

```powershell
cd F:\code\oxipresso
cargo fmt --all
cargo fmt --all --check
cargo test --workspace
cargo test -p oxipresso-engine-xetex
cargo test -p oxipresso-vfs
cargo check -p oxipresso-engine-xetex-sys
cargo test -p oxipresso-viewer --features gui
cargo test -p oxipresso-cli --features gui --bin oxipresso-viewer
cargo test -p oxipresso-render --features pdfium
cargo test -p oxipresso-platform
cargo test -p oxipresso-cli stream_register_open_resume_reads_editor_root
cargo test -p oxipresso-cli stream_register_promised_file_then_open_triggers_rebuild
cargo test -p oxipresso-vfs opening_failed_lookup_file_reports_change_from_start
cargo test -p oxipresso-cli opening_failed_lookup_file_triggers_rebuild
cargo test -p oxipresso-cli line_output_diagnostics_use_line_messages
cargo test -p oxipresso-cli json_protocol_initialization_serializes_messages_as_json
cargo test -p oxipresso-synctex forward_search_path_matches_windows_and_relative_variants
cargo test -p oxipresso-cli synctex_forward_matches_editor_path_variants
cargo test -p oxipresso-render dvi_like_rule_opcodes_render_dark_pixels
cargo test -p oxipresso-render dvi_like_glyph_opcodes_render_placeholder_marks
cargo test -p oxipresso-render dvi_like_font_def_scales_glyph_placeholders
cargo check -p oxipresso-cli --features gui --bin oxipresso-viewer
cargo check -p oxipresso-cli --features "gui pdfium" --bin oxipresso-viewer
```

Result:

- Formatting passed.
- Workspace tests passed.
- GUI viewer crate feature tests passed.
- GUI viewer binary tests passed.
- PDFium render feature tests passed.
- GUI + PDFium feature check passed.
- GUI feature check passed for `oxipresso-viewer`.
- Native C FFI stub compiled successfully on Windows.
- Current unit test coverage includes:
  - CLI option parsing.
  - provider flag conflict.
  - S-expression command parsing.
  - JSON command parsing.
  - editor message serialization.
  - FFI stub initialization.
  - FFI stub callback bridge into `EngineIo`.
  - external `xelatex` backend can compile a simple `.tex` into a PDF artifact when `xelatex` is available.
  - external `xelatex` backend can compile the original TeXpresso `test/simple.tex` fixture when both `xelatex` and the original repo are available.
  - VFS byte change.
  - VFS line change.
  - VFS UTF-16 range change.
  - promised file open behavior.
  - stream `register` does not emit until engine lookup.
  - `resume` emits promised lookup after engine request.
  - stream `register` + `open` + `resume` initializes from editor VFS root content and emits successful lookup/input-file messages.
  - stream registered include files become promised lookups on resume and rebuild successfully after the editor opens the promised file.
  - promised file fulfillment reports a change from offset 0.
  - failed lookup-file fulfillment reports a change from offset 0.
  - CLI rebuilds after the editor opens a previously failed lookup file and the backend then sees it as a successful input.
  - VFS reads from disk roots and keeps editor buffers ahead of disk content.
  - VFS records `input-file` events once per opened input path.
  - `EngineIo::snapshot_inputs()` excludes engine output and exports editor/disk input content.
  - external `xelatex` backend compiles an include file provided only through `EngineIo::snapshot_inputs()`.
  - external `xelatex` backend captures a real SyncTeX artifact and `oxipresso-synctex` parses its input metadata.
  - external `xelatex` backend compiles the original TeXpresso `include.tex` fixture through CLI `-I` when `xelatex` and fixtures are available.
  - external `xelatex` backend compiles the original TeXpresso `includegraphics.tex` fixture when image assets are available.
  - CLI external backend reports original TeXpresso `missing-input.tex` diagnostics without failing the live session.
  - CLI `-lines` diagnostics use `truncate-lines`/`append-lines`/`flush`.
  - CLI JSON protocol initialization emits JSON wire messages end-to-end.
  - CLI persists SyncTeX bytes to `OXIPRESSO_SYNCTEX_OUT` when requested.
  - CLI `synctex-forward` updates internal viewer page state and SyncTeX marker coordinates from latest parsed SyncTeX data.
  - CLI `synctex-forward` accepts editor path variants that match Windows-style absolute SyncTeX input paths.
  - SyncTeX forward lookup matches Windows backslash paths and absolute/relative path variants.
  - GUI marker coordinate mapping has a test for TeX-point-to-page placement; `oxipresso-synctex` also tests reverse nearest-point lookup.
  - CLI emits an `input-file` message for the root read during initialization.
  - CLI editor changes trigger a first-stage full-restart rebuild and refresh viewer state.
  - CLI seeds VFS disk roots from root dir and include paths.
  - render metadata counts DVI/XDV pages using a lightweight opcode walker.
  - render parses DVI/XDV rule opcodes and paints them as dark rectangles.
  - render parses basic DVI/XDV glyph opcodes and paints them as placeholder marks.
  - render parses DVI font definitions/font selection and scales glyph placeholders from font scaled size.
  - viewer page navigation bounds.
  - PDF header validation.
  - PDF page-count scanning that distinguishes `/Page` from `/Pages`.
  - placeholder page output dimensions and buffer size.
  - `AutoRenderBackend` fallback to metadata placeholder.
  - viewer artifact loading through a render backend.
  - CLI initialization refreshes viewer state from an engine-produced artifact.
  - artifact kind detection shared from `oxipresso-viewer`.
  - platform polling file watcher reports created/changed files and keeps changes pending until marked clean.
  - GUI watch mode can start before the artifact exists.
  - GUI watch mode loads a newly created artifact.
  - GUI watch mode keeps the previous artifact when a reload fails validation.
- GUI viewer binary artifact path parsing and artifact kind detection.
- optional GUI viewer code compiles with `eframe/egui` + `wgpu`.
- optional PDFium render code compiles.
- FFI stub callback coverage now includes `size`, `flush`, `diagnostic`, and mirrored XDV artifact output.
- FFI lookup kind mapping is covered for key Tectonic file-format enum values.
- Default `oxipresso-engine-xetex-sys` check passed after adding real-mode source exclusions.
- Opt-in real XeTeX smoke test is present and skips unless `OXIPRESSO_USE_REAL_XETEX=1`, `TEXPRESSO_SRC`, and `OXIPRESSO_XETEX_FORMAT` are all set.

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
- In real mode the two stub-behavior tests (`ffi_stub_initializes`, `ffi_stub_uses_engine_io_callbacks`) fail as expected because `oxipresso_xetex_run` is the real engine rather than the stub; the file-kind mapping test and the opt-in smoke test (which skips without an existing format file) pass.
- vcpkg environment notes for this machine: `VCPKG_ROOT=F:\code\vcpkg` must be set for real-mode builds; the vcpkg tree needed manual source downloads for `meson`, `harfbuzz`, and `icu` tarballs into `F:\code\vcpkg\downloads` because vcpkg's bundled curl fails TLS through the local proxy (SOCKS5 `127.0.0.1:10808`); direct `curl.exe` to `codeload.github.com` and `github.com` release assets works and cached tarballs were hash-verified.
- This does not affect default stub builds, which remain dependency-free.

Historical note: before the vcpkg setup, real mode failed early with a `freetype2` vcpkg discovery error; that blocker is now resolved.

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

## Not Done Yet

- Real TeXpresso/XeTeX engine now compiles and links, but it has not yet typeset anything: no real `.fmt` format file exists for the shim to load, so `XetexEngine::initialize` in real mode still ends at the engine's "format not found" abort path.
- The next real-engine milestone is producing or locating a loadable format (e.g. building `texpresso.fmt` the way TeXpresso does, or trying a compatible TinyTeX `xelatex.fmt`), then running the opt-in real smoke test on `test/simple.tex` to produce a first real XDV artifact through `EngineIo`.
- In real mode the stub-behavior tests in `oxipresso-engine-xetex` fail by design (`oxipresso_xetex_run` is no longer the stub); they either need a stub/real selection guard or to be split into stub-only and real-only variants.
- The FFI stub reads bytes through callback I/O, but it does not parse TeX or produce `.xdv`, `.pdf`, `.log`, or SyncTeX output.
- CLI can perform real LaTeX compilation through `OXIPRESSO_ENGINE=external`, but the default FFI path still uses the stub.
- CLI now emits basic `truncate`/`append`/`flush` messages from engine diagnostics, but does not yet split stdout/log streams like TeXpresso.
- CLI now emits basic `input-file` messages from VFS opens, but it does not yet mirror every TeXpresso input-file/indexing nuance from the original engine.
- CLI can persist SyncTeX sidecar bytes and `oxipresso-synctex` can parse input/output/page/node metadata plus first forward/reverse lookup helpers. CLI now acts on `synctex-forward` at the page/marker level, but precise GUI coordinate scrolling and user-triggered reverse SyncTeX are not implemented yet.
- CLI can write the produced PDF artifact to disk through `OXIPRESSO_ARTIFACT_OUT` and now updates internal `ViewerState`; a separate `oxipresso-viewer --watch <artifact>` process can watch that artifact path through the platform watcher abstraction.
- The current live-preview bridge is polling-based artifact reload, not a direct editor-protocol or engine-event connection.
- `oxipresso-viewer` is not yet a true TeXpresso live preview process connected to editor protocol, SyncTeX, or engine events.
- `oxipresso-render` can render real PDF pages only when the optional `pdfium` feature is enabled; default builds still use placeholder fallback.
- XDV/DVI page metadata can count pages, rule opcodes can render as rectangles, basic glyph opcodes can render as placeholder marks, and DVI font definitions can size those placeholders, but real glyph rasterization/font lookup/image/special rendering is not implemented yet.
- The PDF page counter is intentionally lightweight and may not see page objects hidden in compressed object streams; it currently falls back to one page for a valid PDF in that case.
- No native platform file watcher is implemented yet; current GUI watch mode is portable polling behind the `oxipresso-platform` watcher abstraction.
- No TeX Live/Tectonic provider is implemented yet.
- No Linux CI/build verification has been run.
- No macOS implementation or verification has been done.
- Original TeXpresso fixture coverage now includes `simple.tex`, `include.tex`, `missing-input.tex`, and `includegraphics.tex` through the external backend when fixtures and `xelatex` are available; protocol snapshots and FFI-backed fixture tests are still pending. The core behaviors from `test_stream.sh`, `test-register.sh`, and the non-blocking missing-file path in `test-lookup-file.sh` now have Rust unit coverage, but full shell-equivalent async snapshots are not complete.

## Important Design Constraints

- Keep core Rust crates platform-neutral.
- Put OS-specific code only in `oxipresso-platform`.
- Do not introduce Unix-only `fork`, `socketpair`, or `SCM_RIGHTS` into the Rust engine path.
- Do not make Linux a special fork-based backend; eventual incrementality should use one checkpoint/restart model across Windows/Linux/macOS.
- Keep `TypesettingEngine` and `EngineIo` stable so the FFI backend can later be replaced by a pure Rust backend.
- Treat the current C shim as a temporary adapter point, not product logic.

## Suggested Next Steps

1. Promote the PDFium-backed PDF render path into the GUI smoke workflow and decide whether Windows builds should enable `pdfium` by default.
2. Expand the XDV/DVI parser/renderer beyond placeholder glyphs and rules: real font loading, specials, images, and page boxes for TeXpresso-style live preview.
3. Replace the current approximate GUI SyncTeX marker mapping with TeXpresso-compatible page media-box coordinate scrolling and connect user-triggered reverse SyncTeX behavior.
4. Replace the portable polling watcher with native Windows/Linux watchers where useful, or add a proper live CLI/engine-to-viewer event path.
5. Produce or locate a real `.fmt` format file for the real engine (build `texpresso.fmt` the way TeXpresso does at first run, or test a compatible TinyTeX `xelatex.fmt`), since vcpkg dependency setup is complete and the real engine already compiles and links.
6. Run the opt-in real `XetexEngine::initialize` on `test/simple.tex` with the format available and make it produce at least one real artifact or diagnostic through `EngineIo`.
7. Guard or split the stub-behavior tests in `oxipresso-engine-xetex` so stub-only assertions do not fail in real mode (e.g. compile-time `cfg(oxipresso_real_xetex)` selection, which `build.rs` already emits).
8. Expand original TeXpresso fixture integration tests:
   - async register lookup and lookup-file restart scenarios.
   - protocol snapshots.
   - fixture runs through the FFI XeTeX backend once the real shim replaces the stub.

## Current Git State Expectation

Expected untracked project files:

- `.gitignore`
- `Cargo.lock`
- `Cargo.toml`
- `README.md`
- `AGENTS.md`
- `crates/`

`target/` should remain ignored.
