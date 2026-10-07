//! Font-file resolution for the classic/native glyph renderer: the engine's
//! font names (TFM names, resolved Windows paths, ...) are turned into font
//! file bytes through `kpsewhich` (TeX Live / TinyTeX), with a Windows
//! system-font last resort for SimSun/SimHei and friends.

use crate::FontResolver;
use std::path::PathBuf;

#[cfg(feature = "freetype")]
pub struct KpseFontResolver {
    kpsewhich: Option<PathBuf>,
}

#[cfg(feature = "freetype")]
impl KpseFontResolver {
    pub fn detect() -> Option<Self> {
        Some(Self {
            kpsewhich: which_kpsewhich(),
        })
    }

    pub fn dummy() -> Self {
        Self { kpsewhich: None }
    }
}

#[cfg(feature = "freetype")]
fn which_kpsewhich() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("OXIPRESSO_KPSEWHICH") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let output = std::process::Command::new("kpsewhich")
        .arg("--version")
        .output()
        .ok()?;
    output.status.success().then(|| PathBuf::from("kpsewhich"))
}

#[cfg(feature = "freetype")]
impl FontResolver for KpseFontResolver {
    fn find_font_file(&mut self, name: &str, extensions: &[&str]) -> Option<Vec<u8>> {
        // The engine sometimes records the font name as the RESOLVED PATH
        // (ctex-fontset-windows emits e.g. `C:/WINDOWS/fonts\simhei.ttf`) —
        // such a name is used verbatim, without a kpsewhich round-trip.
        let normalized = name.replace('\\', "/");
        let direct = PathBuf::from(&normalized);
        if direct.is_file() {
            return std::fs::read(&direct).ok();
        }
        let bare = normalized
            .rsplit('/')
            .next()
            .unwrap_or(&normalized)
            .to_string();
        let kpsewhich = self.kpsewhich.as_ref()?;
        for extension in extensions {
            let output = std::process::Command::new(kpsewhich)
                .arg(format!("{name}.{extension}"))
                .output()
                .ok()?;
            if !output.status.success() {
                continue;
            }
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let resolved = stdout.lines().map(str::trim).find(|line| !line.is_empty());
            if let Some(path) = resolved
                && let Ok(bytes) = std::fs::read(path)
            {
                return Some(bytes);
            }
        }
        // Last resort: the Windows system font directory by bare name —
        // SimSun/SimHei and friends live there, outside kpathsea databases.
        for extension in extensions {
            let candidate = PathBuf::from(r"C:\Windows\Fonts").join(format!("{bare}.{extension}"));
            if candidate.is_file() {
                return std::fs::read(&candidate).ok();
            }
        }
        None
    }
}
