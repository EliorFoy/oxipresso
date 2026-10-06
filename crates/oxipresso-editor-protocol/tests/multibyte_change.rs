//! Regression: multibyte (CJK) content in change commands must survive the
//! sexp round trip byte-for-byte — the byte-as-char bug mojibaked every
//! Chinese document into Latin-1 soup.

use oxipresso_editor_protocol::{WireProtocol, parse_command};

#[test]
fn change_command_preserves_multibyte_content() {
    let doc = "\\begin{document}\n运动想象脑电（MI-EEG）分类需要同时捕捉 μ、β 节律变化。\n\\end{document}";
    let line = format!(
        "(change \"demo.tex\" 0 87 \"{}\")",
        doc.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    );
    let command = parse_command(&line, WireProtocol::Sexp).expect("parse");
    let oxipresso_editor_protocol::EditorCommand::Change { path, change } = &command else {
        panic!("expected Change, got {command:?}");
    };
    assert_eq!(path, "demo.tex");
    let oxipresso_editor_protocol::Change::Bytes { data, .. } = change else {
        panic!("expected Bytes change, got {change:?}");
    };
    let decoded = String::from_utf8(data.clone()).expect("decoded utf8");
    assert_eq!(decoded, doc, "the decoded insert must equal the original");
}
