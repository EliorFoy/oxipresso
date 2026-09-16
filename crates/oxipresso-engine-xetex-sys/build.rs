use std::{
    env, fs,
    path::{Path, PathBuf},
};

const REAL_DEPS: &[&str] = &[
    "freetype2",
    "harfbuzz",
    "graphite2",
    "fontconfig",
    "libpng",
    "icu-uc",
    "zlib",
];

// vcpkg package (port) names differ from pkg-config names for some libraries:
// `freetype2` installs as the `freetype` port and `icu-uc` is provided by the
// `icu` port, so Windows discovery must use the vcpkg port names.
const REAL_VCPKG_DEPS: &[&str] = &[
    "freetype",
    "harfbuzz",
    "graphite2",
    "fontconfig",
    "libpng",
    "icu",
    "zlib",
];

fn main() {
    println!("cargo:rerun-if-env-changed=OXIPRESSO_USE_REAL_XETEX");
    println!("cargo:rerun-if-env-changed=TEXPRESSO_SRC");
    println!("cargo:rerun-if-env-changed=OXIPRESSO_XETEX_FORMAT");
    println!("cargo:rustc-check-cfg=cfg(oxipresso_real_xetex)");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if env::var("OXIPRESSO_USE_REAL_XETEX").ok().as_deref() == Some("1") {
        build_real(&target_os);
    } else {
        build_stub(&target_os);
    }
}

fn build_stub(target_os: &str) {
    println!("cargo:rerun-if-changed=c/oxipresso_xetex_stub.c");
    let mut build = cc::Build::new();
    build.file("c/oxipresso_xetex_stub.c");
    build.warnings(false);
    define_target(&mut build, target_os);
    build.compile("oxipresso_xetex");
}

fn build_real(target_os: &str) {
    let source_root = texpresso_source_root();
    let engine_root = source_root.join("src").join("engine");
    if !engine_root.is_dir() {
        panic!(
            "OXIPRESSO_USE_REAL_XETEX=1 was set, but TeXpresso engine sources were not found at {}. Set TEXPRESSO_SRC to the original repository root.",
            engine_root.display()
        );
    }

    println!("cargo:rustc-cfg=oxipresso_real_xetex");
    println!("cargo:rerun-if-changed=c/oxipresso_xetex_real.c");
    rerun_for_sources(&engine_root);

    let dep_includes = match target_os {
        "windows" => windows_dep_includes(),
        "linux" => pkg_config_dep_includes(),
        "macos" => configure_macos_placeholder(),
        other => panic!("real XeTeX FFI is not configured for target OS `{other}`"),
    };

    let mut c_build = cc::Build::new();
    c_build.file("c/oxipresso_xetex_real.c");
    add_sources(&mut c_build, &engine_root.join("engine"), &["c"], target_os);
    add_sources(&mut c_build, &engine_root.join("dpx"), &["c"], target_os);
    c_build.file(engine_root.join("main").join("zlib_md5.c"));
    configure_real_build(&mut c_build, &engine_root, &dep_includes, target_os);
    c_build.compile("oxipresso_xetex_c");

    let mut cpp_build = cc::Build::new();
    add_sources(
        &mut cpp_build,
        &engine_root.join("engine"),
        &["cpp"],
        target_os,
    );
    add_sources(
        &mut cpp_build,
        &engine_root.join("layout"),
        &["cpp"],
        target_os,
    );
    cpp_build.cpp(true);
    // `std()` maps to `/std:c++17` on MSVC (a raw `-std=c++17` flag is
    // rejected by cl.exe and would be dropped by flag_if_supported).
    cpp_build.std("c++17");
    configure_real_build(&mut cpp_build, &engine_root, &dep_includes, target_os);
    cpp_build.compile("oxipresso_xetex_cpp");

    // ICU's Windows time-zone detection references the Win32 registry API,
    // which the vcpkg link metadata does not pull in.
    if target_os == "windows" {
        println!("cargo:rustc-link-lib=advapi32");
    }
}

fn texpresso_source_root() -> PathBuf {
    env::var_os("TEXPRESSO_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"F:\code\texpresso-src"))
}

fn define_target(build: &mut cc::Build, target_os: &str) {
    match target_os {
        "windows" => {
            build.define("OXIPRESSO_TARGET_WINDOWS", None);
        }
        "linux" => {
            build.define("OXIPRESSO_TARGET_LINUX", None);
        }
        "macos" => {
            build.define("OXIPRESSO_TARGET_MACOS", None);
            build.define("XETEX_MAC", None);
        }
        _ => {}
    }
}

fn add_sources(build: &mut cc::Build, dir: &Path, extensions: &[&str], target_os: &str) {
    for entry in fs::read_dir(dir).unwrap_or_else(|error| {
        panic!("failed to read source directory {}: {error}", dir.display())
    }) {
        let path = entry.expect("failed to read source directory entry").path();
        if path.is_file()
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extensions.contains(&extension))
            && !is_excluded_real_source(&path, target_os)
        {
            println!("cargo:rerun-if-changed={}", path.display());
            build.file(path);
        }
    }
}

fn is_excluded_real_source(path: &Path, target_os: &str) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    match name {
        "main.c" | "fork.c" | "texpresso_protocol.c" | "formats.c" => true,
        "xetex-macos.c" | "xetex-XeTeXFontInst_Mac.cpp" | "xetex-XeTeXFontMgr_Mac.mm"
            if target_os != "macos" =>
        {
            true
        }
        _ => false,
    }
}

fn configure_real_build(
    build: &mut cc::Build,
    engine_root: &Path,
    dep_includes: &[PathBuf],
    target_os: &str,
) {
    build.include(engine_root);
    build.include(engine_root.join("include"));
    build.include(engine_root.join("engine"));
    build.include(engine_root.join("layout"));
    build.include(engine_root.join("dpx"));
    build.include(engine_root.join("main"));
    for include in dep_includes {
        build.include(include);
    }
    build.warnings(false);
    define_target(build, target_os);
}

fn rerun_for_sources(engine_root: &Path) {
    for relative in ["engine", "layout", "dpx", "include", "main"] {
        let dir = engine_root.join(relative);
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir).unwrap_or_else(|error| {
            panic!("failed to read source directory {}: {error}", dir.display())
        }) {
            let path = entry.expect("failed to read source directory entry").path();
            if path.is_file() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}

fn windows_dep_includes() -> Vec<PathBuf> {
    let mut includes = Vec::new();
    for package in REAL_VCPKG_DEPS {
        // Static libraries with the dynamic CRT (`x64-windows-static-md`) are
        // the vcpkg-rs default triplet for MSVC targets and let the engine be
        // linked into the Rust binary without shipping vcpkg DLLs.
        let library = match vcpkg::Config::new()
            .emit_includes(true)
            .target_triplet("x64-windows-static-md")
            .find_package(package)
        {
            Ok(library) => library,
            Err(error) => {
                panic!(
                    "OXIPRESSO_USE_REAL_XETEX=1 requires vcpkg package `{package}` on Windows: {error}. \
Install it with `vcpkg install {package} --triplet x64-windows-static-md` and set VCPKG_ROOT to the vcpkg checkout."
                );
            }
        };
        for include_path in library.include_paths {
            includes.push(include_path);
        }
    }
    includes
}

fn pkg_config_dep_includes() -> Vec<PathBuf> {
    let mut includes = Vec::new();
    for package in REAL_DEPS {
        let library = pkg_config::Config::new()
            .probe(package)
            .unwrap_or_else(|error| {
                panic!(
                    "OXIPRESSO_USE_REAL_XETEX=1 requires pkg-config package `{package}` on Linux: {error}"
                )
            });
        for include_path in library.include_paths {
            includes.push(include_path);
        }
    }
    println!("cargo:rustc-link-lib=m");
    includes
}

fn configure_macos_placeholder() -> Vec<PathBuf> {
    panic!(
        "OXIPRESSO_USE_REAL_XETEX=1 is not implemented for macOS yet; macOS real XeTeX FFI is planned after Windows and Linux"
    );
}
