//! The Oxipresso extension for Zed: registers the `oxipresso-lsp` language
//! server for LaTeX buffers. The server drives the Oxipresso render
//! server's resident engine (incremental re-typesetting per keystroke),
//! publishes TeX errors as diagnostics, and writes the artifact / SyncTeX
//! sidecar / edit-line file that the companion preview (the standalone
//! Oxipresso Editor Client, or any watcher) renders live.

use zed_extension_api::{self as zed, Command, LanguageServerId, Result};

const SERVER_ID: &str = "oxipresso";

struct OxipressoExtension;

impl zed::Extension for OxipressoExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Command> {
        if language_server_id.as_ref() != SERVER_ID {
            return Err(format!("oxipresso: unsupported server {language_server_id}"));
        }

        // Resolution order: the OXIPRESSO_LSP env var, the worktree, then
        // PATH. The binary ships with the Oxipresso render server.
        if let Some(path) = std::env::var_os("OXIPRESSO_LSP") {
            return Ok(Command {
                command: path.to_string_lossy().into_owned(),
                args: vec![],
                env: vec![],
            });
        }
        if let Some(path) = worktree.which("oxipresso-lsp") {
            return Ok(Command {
                command: path,
                args: vec![],
                env: vec![],
            });
        }
        Err(format!(
            "oxipresso: `oxipresso-lsp` not found. Install the Oxipresso render server \
             (https://github.com/EliorFoy/oxipresso), put `oxipresso-lsp` on PATH, or set \
             the OXIPRESSO_LSP environment variable to its full path."
        ))
    }
}

zed::register_extension!(OxipressoExtension);
