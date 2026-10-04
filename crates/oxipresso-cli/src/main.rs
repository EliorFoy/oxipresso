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

fn main() {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("oxipresso: {error}");
            eprintln!(
                "Usage: oxipresso [-I path]* [-json] [-lines] [-gui] [-texlive] [-tectonic] [-test-initialize] [-stream] root_file.tex"
            );
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
