use std::path::PathBuf;

use oxipresso_viewer::{ViewerGuiOptions, load_artifact_from_path, run_native_viewer};

fn main() {
    let args = match parse_viewer_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("oxipresso-viewer: {error}");
            eprintln!("Usage: oxipresso-viewer [--watch] artifact.pdf|artifact.xdv|artifact.dvi");
            std::process::exit(2);
        }
    };

    let options = if args.watch {
        ViewerGuiOptions::watching(args.artifact_path)
    } else {
        match load_artifact_from_path(&args.artifact_path) {
            Ok(artifact) => ViewerGuiOptions::with_artifact(artifact),
            Err(error) => {
                eprintln!("oxipresso-viewer: {error}");
                std::process::exit(1);
            }
        }
    };

    if let Err(error) = run_native_viewer(options) {
        eprintln!("oxipresso-viewer: {error}");
        std::process::exit(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewerArgs {
    artifact_path: PathBuf,
    watch: bool,
}

fn parse_viewer_args<I, S>(args: I) -> Result<ViewerArgs, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut artifact_path = None;
    let mut watch = false;
    for arg in args.into_iter().map(Into::into) {
        if arg == "--watch" {
            watch = true;
            continue;
        }
        if arg.starts_with('-') {
            return Err(format!("unknown option {arg}"));
        }
        if artifact_path.replace(PathBuf::from(arg)).is_some() {
            return Err("expected a single artifact path".to_string());
        }
    }
    Ok(ViewerArgs {
        artifact_path: artifact_path.ok_or_else(|| "missing artifact path".to_string())?,
        watch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use oxipresso_engine_api::ArtifactKind;
    use oxipresso_viewer::artifact_kind_from_path_and_bytes;

    #[test]
    fn parses_single_artifact_path() {
        assert_eq!(
            parse_viewer_args(["out.pdf"]).unwrap(),
            ViewerArgs {
                artifact_path: PathBuf::from("out.pdf"),
                watch: false,
            }
        );
        assert_eq!(
            parse_viewer_args(["--watch", "out.pdf"]).unwrap(),
            ViewerArgs {
                artifact_path: PathBuf::from("out.pdf"),
                watch: true,
            }
        );
        assert!(parse_viewer_args(["one.pdf", "two.pdf"]).is_err());
        assert!(parse_viewer_args(["--bad"]).is_err());
    }

    #[test]
    fn detects_artifact_kind_from_extension_or_header() {
        assert_eq!(
            artifact_kind_from_path_and_bytes(Path::new("out.pdf"), b"anything"),
            ArtifactKind::Pdf
        );
        assert_eq!(
            artifact_kind_from_path_and_bytes(Path::new("out.xdv"), b""),
            ArtifactKind::Xdv
        );
        assert_eq!(
            artifact_kind_from_path_and_bytes(Path::new("out.dvi"), b""),
            ArtifactKind::Dvi
        );
        assert_eq!(
            artifact_kind_from_path_and_bytes(Path::new("out.bin"), b"\n%PDF-1.7\n"),
            ArtifactKind::Pdf
        );
        assert_eq!(
            artifact_kind_from_path_and_bytes(Path::new("out.bin"), b"nope"),
            ArtifactKind::Unknown
        );
    }
}
