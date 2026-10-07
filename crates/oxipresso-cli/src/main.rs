use std::io::{self, IsTerminal, Write};

use oxipresso_cli::{parse_args, run_with_io};

/// A console window launched by double-clicking the exe closes the moment
/// the process exits, so an error print reads as a "flash and crash".
/// When stdin is an interactive terminal, hold the window open until
/// Enter; piped/CI stdin (non-terminal) never pauses.
fn pause_if_interactive() {
    if io::stdin().is_terminal() {
        eprint!("Press Enter to exit...");
        let _ = io::stderr().flush();
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
    }
}

/// `oxipresso render-page <xdv> <page> <out.png>`: headless page rasterizer
/// for external editor frontends (the neovim client renders the current
/// page through this instead of linking the renderer itself).
/// Options: `--scale <f>` (default 1.5 ≈ 108dpi), `--roots <dir[,dir...]>`
/// (image lookup roots; defaults to the XDV's directory).
#[cfg(feature = "freetype")]
fn run_render_page(args: &[String]) -> Result<(), String> {
    let mut xdv: Option<String> = None;
    let mut page: Option<usize> = None;
    let mut out: Option<String> = None;
    let mut scale: f32 = 1.5;
    let mut roots: Vec<std::path::PathBuf> = Vec::new();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--scale" => {
                scale = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| "render-page: --scale needs a number".to_string())?;
            }
            "--roots" => {
                let list = it
                    .next()
                    .ok_or_else(|| "render-page: --roots needs a value".to_string())?;
                roots = list
                    .split(',')
                    .map(std::path::PathBuf::from)
                    .collect();
            }
            "--page" => {
                page = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| "render-page: --page needs a number".to_string())?;
            }
            other if xdv.is_none() => xdv = Some(other.to_string()),
            other if page.is_none() => {
                page = Some(other.parse::<usize>().map_err(|_| {
                    format!("render-page: invalid page number {other}")
                })?);
            }
            other if out.is_none() => out = Some(other.to_string()),
            other => return Err(format!("render-page: unexpected argument {other}")),
        }
    }
    let xdv = xdv.ok_or_else(|| "render-page: missing the XDV path".to_string())?;
    let page = page.ok_or_else(|| "render-page: missing the page number".to_string())?;
    let out = out.ok_or_else(|| "render-page: missing the PNG output path".to_string())?;

    use oxipresso_engine_api::{ArtifactKind, DocumentArtifact};
    use oxipresso_render::{RenderBackend, XdvGlyphRenderBackend};

    let bytes = std::fs::read(&xdv)
        .map_err(|e| format!("render-page: cannot read {xdv}: {e}"))?;
    let artifact = DocumentArtifact {
        kind: ArtifactKind::Xdv,
        bytes,
        source_name: Some(xdv.clone()),
    };

    // Image lookup roots default to the XDV's directory (figures live next
    // to the document).
    if roots.is_empty()
        && let Some(parent) = std::path::Path::new(&xdv).parent()
    {
        roots.push(parent.to_path_buf());
    }
    let loader = oxipresso_cli::DocumentImageLoader { roots };
    let resolver = oxipresso_cli::KpseFontResolver::detect()
        .unwrap_or_else(|| oxipresso_cli::KpseFontResolver::dummy());
    let backend = XdvGlyphRenderBackend::with_image_loader(
        Box::new(resolver),
        Box::new(loader),
    );

    let rendered = backend
        .render_page_scaled(&artifact, page.saturating_sub(1), scale)
        .map_err(|e| format!("render-page: {e}"))?;

    let file = std::fs::File::create(&out)
        .map_err(|e| format!("render-page: cannot create {out}: {e}"))?;
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        rendered.width,
        rendered.height,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("render-page: PNG header failed: {e}"))?;
    writer
        .write_image_data(&rendered.pixels_rgba)
        .map_err(|e| format!("render-page: PNG write failed: {e}"))?;
    eprintln!(
        "render-page: {}x{} -> {out}",
        rendered.width, rendered.height
    );
    Ok(())
}

fn main() {
    let all_args: Vec<String> = std::env::args().skip(1).collect();

    // Headless page rasterizer for editor frontends (freetype builds only).
    #[cfg(feature = "freetype")]
    if all_args.first().map(String::as_str) == Some("render-page") {
        let code = match run_render_page(&all_args[1..]) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("oxipresso: {error}");
                1
            }
        };
        std::process::exit(code);
    }
    #[cfg(not(feature = "freetype"))]
    if all_args.first().map(String::as_str) == Some("render-page") {
        eprintln!("oxipresso: render-page requires building with the `freetype` feature");
        std::process::exit(2);
    }

    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("oxipresso: {error}");
            eprintln!(
                "Usage: oxipresso [-I path]* [-json] [-lines] [-gui] [-texlive] [-tectonic] [-test-initialize] [-stream] root_file.tex"
            );
            eprintln!("       oxipresso render-page <xdv> <page> <out.png> [--scale f]");
            eprintln!("(tip: drag a .tex file onto oxipresso.exe to open it)");
            pause_if_interactive();
            std::process::exit(2);
        }
    };

    #[cfg(feature = "gui")]
    if options.gui {
        if let Err(error) = oxipresso_cli::run_live_preview(options) {
            eprintln!("oxipresso: {error}");
            pause_if_interactive();
            std::process::exit(1);
        }
        return;
    }

    if options.gui {
        eprintln!("oxipresso: -gui requires building with the `gui` feature");
        pause_if_interactive();
        std::process::exit(2);
    }

    if let Err(error) = run_with_io(options, io::stdin().lock(), io::stdout().lock()) {
        eprintln!("oxipresso: {error}");
        pause_if_interactive();
        std::process::exit(1);
    }
}
