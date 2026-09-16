use std::io;

use oxipresso_cli::{parse_args, run_with_io};

fn main() {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("oxipresso: {error}");
            eprintln!(
                "Usage: oxipresso [-I path]* [-json] [-lines] [-gui] [-texlive] [-tectonic] [-test-initialize] [-stream] root_file.tex"
            );
            std::process::exit(2);
        }
    };

    #[cfg(feature = "gui")]
    if options.gui {
        if let Err(error) = oxipresso_cli::run_live_preview(options) {
            eprintln!("oxipresso: {error}");
            std::process::exit(1);
        }
        return;
    }

    if options.gui {
        eprintln!("oxipresso: -gui requires building with the `gui` feature");
        std::process::exit(2);
    }

    if let Err(error) = run_with_io(options, io::stdin().lock(), io::stdout().lock()) {
        eprintln!("oxipresso: {error}");
        std::process::exit(1);
    }
}
