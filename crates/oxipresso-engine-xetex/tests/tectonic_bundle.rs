//! Tectonic local-bundle resolver tests (P3).

use oxipresso_engine_api::{FileKind, FileResolver};
use oxipresso_engine_xetex::tectonic::DirBundleResolver;

#[test]
fn resolves_bundle_paths_and_rejects_escapes() {
    let dir = std::env::temp_dir().join(format!(
        "oxi-tectonic-bundle-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let texmf = dir.join("texmf");
    std::fs::create_dir_all(texmf.join("fonts/tfm/public/cm")).unwrap();
    std::fs::create_dir_all(texmf.join("tex/plain/base")).unwrap();
    std::fs::write(texmf.join("tex/plain/base/plain.tex"), b"\\relax").unwrap();
    std::fs::write(
        texmf.join("fonts/tfm/public/cm/cmr10.tfm"),
        b"fake tfm bytes",
    )
    .unwrap();

    let mut resolver = DirBundleResolver::new(texmf);

    // Exact-path hit.
    let hit = resolver.resolve("tex/plain/base/plain.tex", FileKind::Tex);
    assert_eq!(hit.as_deref(), Some(b"\\relax".as_slice()), "tex hit");

    // Extension guess for extensionless TFM lookups.
    let tfm = resolver.resolve("fonts/tfm/public/cm/cmr10", FileKind::Tfm);
    assert_eq!(
        tfm.as_deref(),
        Some(b"fake tfm bytes".as_slice()),
        "tfm guess"
    );

    // Miss: an unknown file resolves to None (the VFS falls through).
    assert!(
        resolver
            .resolve("tex/generic/none.tex", FileKind::Tex)
            .is_none()
    );

    // Escape attempts never resolve.
    assert!(resolver.resolve("../outside.tex", FileKind::Tex).is_none());
    assert!(resolver.resolve("/abs/path.tex", FileKind::Tex).is_none());
    assert!(
        resolver
            .resolve("fonts/../../outside.tex", FileKind::Tex)
            .is_none()
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn check_env_reports_missing_directory() {
    // Unset env: a clear error naming the variable (never a panic).
    let saved = std::env::var_os(oxipresso_engine_xetex::tectonic::BUNDLE_ENV);
    unsafe {
        std::env::remove_var(oxipresso_engine_xetex::tectonic::BUNDLE_ENV);
    }
    let err = DirBundleResolver::check_env().unwrap_err();
    assert!(
        err.contains(oxipresso_engine_xetex::tectonic::BUNDLE_ENV),
        "error names the env: {err}"
    );
    if let Some(value) = saved {
        unsafe {
            std::env::set_var(oxipresso_engine_xetex::tectonic::BUNDLE_ENV, value);
        }
    }
}
