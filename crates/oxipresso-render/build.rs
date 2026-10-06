fn main() {
    // Only the opt-in `freetype` feature needs the native FreeType library;
    // default builds stay dependency-free.
    #[cfg(feature = "freetype")]
    {
        let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        match target_os.as_str() {
            "windows" => {
                let library = vcpkg::Config::new()
                    .target_triplet("x64-windows-static-md")
                    .find_package("freetype")
                    .unwrap_or_else(|error| {
                        panic!(
                            "the oxipresso-render `freetype` feature requires the vcpkg package \
                             `freetype` on Windows: {error}. Install it with \
                             `vcpkg install freetype --triplet x64-windows-static-md` and set VCPKG_ROOT."
                        )
                    });
                // Field accessors compiled against the real headers (the Rust
                // side is layout-independent and must not scan face memory).
                let mut build = cc::Build::new();
                build.file("c/ft_helpers.c");
                for include in library.include_paths {
                    build.include(include);
                }
                build.compile("oxipresso_ft_helpers");
            }
            "linux" => {
                let library = pkg_config::Config::new()
                    .probe("freetype2")
                    .unwrap_or_else(|error| {
                        panic!(
                            "the oxipresso-render `freetype` feature requires the pkg-config package \
                             `freetype2` on Linux: {error}"
                        )
                    });
                let _ = library;
            }
            other => panic!(
                "the oxipresso-render `freetype` feature is not configured for target OS `{other}` yet"
            ),
        }
    }
}
