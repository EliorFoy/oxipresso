//! P1: the shipped binary speaks the editor wire end to end from OUTSIDE the
//! Rust process — the reference client's contract, pinned as a cargo test
//! (initialize stream, an editor change driving a full rebuild, and
//! pause/resume folding into a rebuild).

use oxipresso_cli::editor_wire;

#[test]
fn editor_wire_selftest_against_shipped_binary() {
    let binary = env!("CARGO_BIN_EXE_oxipresso");
    editor_wire::run_wire_selftest(std::path::Path::new(binary))
        .expect("the shipped binary must pass the editor wire selftest");
}
