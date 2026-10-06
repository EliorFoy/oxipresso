use std::fmt;

pub type Result<T> = std::result::Result<T, ProtocolError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    message: String,
}

impl ProtocolError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireProtocol {
    Sexp,
    Json,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditorCommand {
    Open {
        path: String,
        data: Vec<u8>,
        base64: bool,
    },
    Close {
        path: String,
    },
    Change {
        path: String,
        change: Change,
    },
    Theme {
        bg: [f32; 3],
        fg: [f32; 3],
    },
    PreviousPage,
    NextPage,
    MoveWindow {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
    MapWindow {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
    UnmapWindow,
    Rescan,
    StayOnTop(bool),
    SynctexForward {
        path: String,
        line: usize,
    },
    Crop,
    Invert,
    Register {
        path: String,
    },
    Pause,
    Resume,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Bytes {
        offset: usize,
        remove: usize,
        data: Vec<u8>,
    },
    Lines {
        offset: usize,
        remove: usize,
        data: Vec<u8>,
    },
    Range {
        start_line: usize,
        start_char: usize,
        end_line: usize,
        end_char: usize,
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoBuffer {
    Out,
    Log,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupKind {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupStatus {
    Successful,
    Failed,
    Promised,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorMessage {
    Truncate {
        buffer: InfoBuffer,
        size: usize,
    },
    Append {
        buffer: InfoBuffer,
        /// Byte offset within the info buffer where this chunk starts, matching
        /// TeXpresso `editor_append`'s `(append <buffer> <pos> "<text>")` arity
        /// (0 after a fresh truncate; the stream cursor for partial chunks).
        pos: usize,
        text: String,
    },
    TruncateLines {
        buffer: InfoBuffer,
        count: usize,
    },
    AppendLines {
        buffer: InfoBuffer,
        lines: Vec<String>,
    },
    Flush,
    Synctex {
        path: String,
        line: usize,
        column: usize,
    },
    ResetSync,
    InputFile {
        index: usize,
        path: String,
    },
    LookupFile {
        kind: LookupKind,
        status: LookupStatus,
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    List(Vec<Value>),
    String(String),
    Atom(String),
    Number(f64),
    Bool(bool),
}

pub fn parse_command(input: &str, protocol: WireProtocol) -> Result<EditorCommand> {
    let value = match protocol {
        WireProtocol::Sexp => {
            let mut parser = SexpParser::new(input);
            let value = parser.parse_value()?;
            parser.skip_ws();
            if !parser.is_done() {
                return Err(ProtocolError::new("trailing input after S-expression"));
            }
            value
        }
        WireProtocol::Json => value_from_json(
            serde_json::from_str(input)
                .map_err(|e| ProtocolError::new(format!("invalid JSON command: {e}")))?,
        )?,
    };
    command_from_value(value, protocol)
}

pub fn serialize_message(message: &EditorMessage, protocol: WireProtocol) -> String {
    let fields = message_fields(message);
    match protocol {
        WireProtocol::Sexp => {
            let mut out = String::from("(");
            for (index, field) in fields.iter().enumerate() {
                if index > 0 {
                    out.push(' ');
                }
                push_sexp_field(&mut out, field);
            }
            out.push(')');
            out
        }
        WireProtocol::Json => {
            serde_json::to_string(&fields.iter().map(Field::to_json).collect::<Vec<_>>())
                .expect("message fields are JSON serializable")
        }
    }
}

#[derive(Debug, Clone)]
enum Field {
    Symbol(&'static str),
    String(String),
    Number(usize),
}

impl Field {
    fn to_json(&self) -> serde_json::Value {
        match self {
            Field::Symbol(s) => serde_json::Value::String((*s).to_string()),
            Field::String(s) => serde_json::Value::String(s.clone()),
            Field::Number(n) => serde_json::Value::Number((*n).into()),
        }
    }
}

fn message_fields(message: &EditorMessage) -> Vec<Field> {
    match message {
        EditorMessage::Truncate { buffer, size } => {
            vec![
                Field::Symbol("truncate"),
                Field::Symbol(buffer_name(*buffer)),
                Field::Number(*size),
            ]
        }
        EditorMessage::Append { buffer, pos, text } => {
            vec![
                Field::Symbol("append"),
                Field::Symbol(buffer_name(*buffer)),
                Field::Number(*pos),
                Field::String(text.clone()),
            ]
        }
        EditorMessage::TruncateLines { buffer, count } => vec![
            Field::Symbol("truncate-lines"),
            Field::Symbol(buffer_name(*buffer)),
            Field::Number(*count),
        ],
        EditorMessage::AppendLines { buffer, lines } => {
            let mut fields = vec![
                Field::Symbol("append-lines"),
                Field::Symbol(buffer_name(*buffer)),
            ];
            fields.extend(lines.iter().cloned().map(Field::String));
            fields
        }
        EditorMessage::Flush => vec![Field::Symbol("flush")],
        EditorMessage::Synctex { path, line, column } => vec![
            Field::Symbol("synctex"),
            Field::String(path.clone()),
            Field::Number(*line),
            Field::Number(*column),
        ],
        EditorMessage::ResetSync => vec![Field::Symbol("reset-sync")],
        EditorMessage::InputFile { index, path } => {
            vec![
                Field::Symbol("input-file"),
                Field::Number(*index),
                Field::String(path.clone()),
            ]
        }
        EditorMessage::LookupFile { kind, status, path } => vec![
            Field::Symbol("lookup-file"),
            Field::Symbol(match kind {
                LookupKind::Read => "read",
                LookupKind::Write => "write",
            }),
            Field::Symbol(match status {
                LookupStatus::Successful => "successful",
                LookupStatus::Failed => "failed",
                LookupStatus::Promised => "promised",
            }),
            Field::String(path.clone()),
        ],
    }
}

fn buffer_name(buffer: InfoBuffer) -> &'static str {
    match buffer {
        InfoBuffer::Out => "out",
        InfoBuffer::Log => "log",
    }
}

fn push_sexp_field(out: &mut String, field: &Field) {
    match field {
        Field::Symbol(s) => out.push_str(s),
        Field::Number(n) => out.push_str(&n.to_string()),
        Field::String(s) => push_sexp_string(out, s),
    }
}

fn push_sexp_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch => out.push(ch),
        }
    }
    out.push('"');
}

fn value_from_json(value: serde_json::Value) -> Result<Value> {
    Ok(match value {
        serde_json::Value::Array(values) => Value::List(
            values
                .into_iter()
                .map(value_from_json)
                .collect::<Result<_>>()?,
        ),
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Number(n) => Value::Number(
            n.as_f64()
                .ok_or_else(|| ProtocolError::new("unsupported JSON number"))?,
        ),
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Null => Value::Atom("nil".to_string()),
        _ => return Err(ProtocolError::new("unsupported JSON command value")),
    })
}

fn command_from_value(value: Value, protocol: WireProtocol) -> Result<EditorCommand> {
    let Value::List(values) = value else {
        return Err(ProtocolError::new("command must be a list"));
    };
    if values.is_empty() {
        return Err(ProtocolError::new("command list must not be empty"));
    }
    let verb = symbol_or_string(&values[0], protocol)?;
    let args = &values[1..];
    match verb.as_str() {
        "open" | "open-base64" => {
            expect_arity(&verb, args, 2)?;
            Ok(EditorCommand::Open {
                path: expect_string(&args[0])?,
                data: expect_string(&args[1])?.into_bytes(),
                base64: verb == "open-base64",
            })
        }
        "close" => {
            expect_arity(&verb, args, 1)?;
            Ok(EditorCommand::Close {
                path: expect_string(&args[0])?,
            })
        }
        "change" => {
            expect_arity(&verb, args, 4)?;
            Ok(EditorCommand::Change {
                path: expect_string(&args[0])?,
                change: Change::Bytes {
                    offset: expect_usize(&args[1])?,
                    remove: expect_usize(&args[2])?,
                    data: expect_string(&args[3])?.into_bytes(),
                },
            })
        }
        "change-lines" => {
            expect_arity(&verb, args, 4)?;
            Ok(EditorCommand::Change {
                path: expect_string(&args[0])?,
                change: Change::Lines {
                    offset: expect_usize(&args[1])?,
                    remove: expect_usize(&args[2])?,
                    data: expect_string(&args[3])?.into_bytes(),
                },
            })
        }
        "change-range" => {
            expect_arity(&verb, args, 6)?;
            Ok(EditorCommand::Change {
                path: expect_string(&args[0])?,
                change: Change::Range {
                    start_line: expect_usize(&args[1])?,
                    start_char: expect_usize(&args[2])?,
                    end_line: expect_usize(&args[3])?,
                    end_char: expect_usize(&args[4])?,
                    data: expect_string(&args[5])?.into_bytes(),
                },
            })
        }
        "theme" => {
            expect_arity(&verb, args, 2)?;
            Ok(EditorCommand::Theme {
                bg: expect_color(&args[0])?,
                fg: expect_color(&args[1])?,
            })
        }
        "previous-page" => {
            expect_arity(&verb, args, 0)?;
            Ok(EditorCommand::PreviousPage)
        }
        "next-page" => {
            expect_arity(&verb, args, 0)?;
            Ok(EditorCommand::NextPage)
        }
        "move-window" => {
            let (x, y, w, h) = parse_rect_args(&verb, args)?;
            Ok(EditorCommand::MoveWindow { x, y, w, h })
        }
        "map-window" => {
            let (x, y, w, h) = parse_rect_args(&verb, args)?;
            Ok(EditorCommand::MapWindow { x, y, w, h })
        }
        "unmap-window" => {
            expect_arity(&verb, args, 0)?;
            Ok(EditorCommand::UnmapWindow)
        }
        "rescan" => {
            expect_arity(&verb, args, 0)?;
            Ok(EditorCommand::Rescan)
        }
        "stay-on-top" => {
            expect_arity(&verb, args, 1)?;
            Ok(EditorCommand::StayOnTop(expect_truthy(&args[0], protocol)?))
        }
        "synctex-forward" => {
            expect_arity(&verb, args, 2)?;
            Ok(EditorCommand::SynctexForward {
                path: expect_string(&args[0])?,
                line: expect_usize(&args[1])?,
            })
        }
        "crop" => {
            expect_arity(&verb, args, 0)?;
            Ok(EditorCommand::Crop)
        }
        "invert" => {
            expect_arity(&verb, args, 0)?;
            Ok(EditorCommand::Invert)
        }
        "register" => {
            expect_arity(&verb, args, 1)?;
            Ok(EditorCommand::Register {
                path: expect_string(&args[0])?,
            })
        }
        "pause" => {
            expect_arity(&verb, args, 0)?;
            Ok(EditorCommand::Pause)
        }
        "resume" => {
            expect_arity(&verb, args, 0)?;
            Ok(EditorCommand::Resume)
        }
        _ => Err(ProtocolError::new(format!("unknown command verb: {verb}"))),
    }
}

fn parse_rect_args(verb: &str, args: &[Value]) -> Result<(f32, f32, f32, f32)> {
    expect_arity(verb, args, 4)?;
    Ok((
        expect_f32(&args[0])?,
        expect_f32(&args[1])?,
        expect_f32(&args[2])?,
        expect_f32(&args[3])?,
    ))
}

fn expect_arity(verb: &str, args: &[Value], expected: usize) -> Result<()> {
    if args.len() != expected {
        Err(ProtocolError::new(format!(
            "{verb}: expected {expected} arguments, got {}",
            args.len()
        )))
    } else {
        Ok(())
    }
}

fn symbol_or_string(value: &Value, protocol: WireProtocol) -> Result<String> {
    match value {
        Value::Atom(s) => Ok(s.clone()),
        Value::String(s) if protocol == WireProtocol::Json => Ok(s.clone()),
        _ => Err(ProtocolError::new("expected command verb")),
    }
}

fn expect_string(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        _ => Err(ProtocolError::new("expected string")),
    }
}

fn expect_usize(value: &Value) -> Result<usize> {
    match value {
        Value::Number(n) if *n >= 0.0 && n.fract() == 0.0 => Ok(*n as usize),
        _ => Err(ProtocolError::new("expected non-negative integer")),
    }
}

fn expect_f32(value: &Value) -> Result<f32> {
    match value {
        Value::Number(n) => Ok(*n as f32),
        _ => Err(ProtocolError::new("expected number")),
    }
}

fn expect_truthy(value: &Value, protocol: WireProtocol) -> Result<bool> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::Atom(s) if protocol == WireProtocol::Sexp => Ok(s != "nil"),
        _ => Err(ProtocolError::new("expected boolean")),
    }
}

fn expect_color(value: &Value) -> Result<[f32; 3]> {
    let Value::List(values) = value else {
        return Err(ProtocolError::new("expected color list"));
    };
    if values.len() != 3 {
        return Err(ProtocolError::new(
            "expected RGB color with three components",
        ));
    }
    Ok([
        expect_f32(&values[0])?,
        expect_f32(&values[1])?,
        expect_f32(&values[2])?,
    ])
}

struct SexpParser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> SexpParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
        }
    }
    fn is_done(&self) -> bool {
        self.pos == self.input.len()
    }
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b) if b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }
    fn parse_value(&mut self) -> Result<Value> {
        self.skip_ws();
        match self.peek() {
            Some(b'(') => self.parse_list(),
            Some(b'"') => self.parse_string().map(Value::String),
            Some(_) => self.parse_atom_or_number(),
            None => Err(ProtocolError::new("unexpected end of input")),
        }
    }
    fn parse_list(&mut self) -> Result<Value> {
        self.expect_byte(b'(')?;
        let mut values = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b')') => {
                    self.pos += 1;
                    return Ok(Value::List(values));
                }
                Some(_) => values.push(self.parse_value()?),
                None => return Err(ProtocolError::new("unterminated list")),
            }
        }
    }
    fn parse_string(&mut self) -> Result<String> {
        self.expect_byte(b'"')?;
        // Byte-collecting: the raw non-ASCII bytes (UTF-8 multibyte) must
        // pass through untouched — pushing each byte `as char` would
        // mojibake every CJK document into Latin-1 soup (36662-byte
        // "changes" for a 23452-byte file).
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(b) = self.next() else {
                return Err(ProtocolError::new("unterminated string"));
            };
            match b {
                b'"' => {
                    return String::from_utf8(out)
                        .map_err(|_| ProtocolError::new("string is not valid UTF-8"));
                }
                b'\\' => {
                    let Some(escaped) = self.next() else {
                        return Err(ProtocolError::new("unterminated escape"));
                    };
                    match escaped {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'\\' => out.push(b'\\'),
                        b'"' => out.push(b'"'),
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
    }
    fn parse_atom_or_number(&mut self) -> Result<Value> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_whitespace() || b == b'(' || b == b')' {
                break;
            }
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.input[start..self.pos])
            .map_err(|_| ProtocolError::new("atom is not valid UTF-8"))?;
        if let Ok(number) = text.parse::<f64>() {
            Ok(Value::Number(number))
        } else if text == "t" {
            Ok(Value::Bool(true))
        } else if text == "nil" {
            Ok(Value::Bool(false))
        } else {
            Ok(Value::Atom(text.to_string()))
        }
    }
    fn expect_byte(&mut self, expected: u8) -> Result<()> {
        match self.next() {
            Some(actual) if actual == expected => Ok(()),
            _ => Err(ProtocolError::new("unexpected byte")),
        }
    }
    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }
    fn next(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers_reject_malformed_input_without_panicking() {
        // The CLI feeds untrusted editor bytes through these parsers; malformed
        // input must produce Err (never panic/crash the session). All cases use
        // known verbs with unambiguous arity/type errors or broken structure.
        let malformed_sexp = [
            "(",
            ")",
            "(open",
            "(open \"unterminated",
            "(open)",
            "(open \"a\")",
            "(open \"a\" \"b\" \"c\")",
            "(change \"a\" 0 0)",
            "(((",
            "( )",
        ];
        let malformed_json = [
            "",
            "not json",
            "[",
            "{}",
            "[1,2,3]",
            "[\"open\"]",
            "[\"open\",\"a\",\"b\",\"c\"]",
            "[\"change\",\"a\",1,2]",
            "[\"change-range\",\"m\",1,2,3]",
        ];
        for input in malformed_sexp {
            assert!(
                parse_command(input, WireProtocol::Sexp).is_err(),
                "sexp input should error, not panic: {input:?}"
            );
        }
        for input in malformed_json {
            assert!(
                parse_command(input, WireProtocol::Json).is_err(),
                "json input should error, not panic: {input:?}"
            );
        }
        // Control: valid commands still parse, proving these aren't blanket errors.
        assert!(parse_command(r#"(open "a.tex" "hi")"#, WireProtocol::Sexp).is_ok());
        assert!(parse_command(r#"["open","a.tex","hi"]"#, WireProtocol::Json).is_ok());
    }

    #[test]
    fn fuzz_parse_command_never_panics() {
        // The CLI feeds raw editor bytes here; fuzz both wire dialects with
        // seeded-random strings over a protocol-relevant alphabet plus a
        // truncation sweep of valid commands (every early-EOF boundary). Err or
        // Ok are both fine; a panic fails the test.
        let alphabet: &[u8] =
            b"()[]{}\",: \n0123456789abcdefghijklmnopqrstuvwxyz-openchange-resume-page-synctextrue.";
        let mut state: u64 = 0xCAFE_BABE_1234_5678;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_F491_4F6F_DD1D) >> 33) as u8
        };
        for _case in 0..30_000usize {
            let len = (next() as usize % 48) + 1;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                buf.push(alphabet[next() as usize % alphabet.len()]);
            }
            let s = String::from_utf8_lossy(&buf).into_owned();
            let _ = parse_command(&s, WireProtocol::Sexp);
            let _ = parse_command(&s, WireProtocol::Json);
        }
        // Truncation sweep (all-ASCII examples, so every byte cut is a char
        // boundary) of valid commands under each protocol.
        for valid in [
            r#"(open "a.tex" "hi")"#,
            r#"(change "m.tex" 0 "x")"#,
            r#"(synctex-forward "m.tex" 10 0)"#,
            r#"(theme 0.1 0.2 0.3 0.4 0.5)"#,
        ] {
            for cut in 0..=valid.len() {
                let _ = parse_command(&valid[..cut], WireProtocol::Sexp);
            }
        }
        for valid in [
            r#"["open","a.tex","hi"]"#,
            r#"["change","m.tex",0,"x"]"#,
            r#"["theme",0.1,0.2,0.3,0.4,0.5]"#,
        ] {
            for cut in 0..=valid.len() {
                let _ = parse_command(&valid[..cut], WireProtocol::Json);
            }
        }
        // Control: valid commands still parse (guards a blanket-error parser).
        assert!(parse_command(r#"(open "a.tex" "hi")"#, WireProtocol::Sexp).is_ok());
        assert!(parse_command(r#"["open","a.tex","hi"]"#, WireProtocol::Json).is_ok());
    }

    #[test]
    fn parses_sexp_change_range() {
        let cmd = parse_command(
            r#"(change-range "main.tex" 1 2 3 4 "hello")"#,
            WireProtocol::Sexp,
        )
        .unwrap();
        assert_eq!(
            cmd,
            EditorCommand::Change {
                path: "main.tex".to_string(),
                change: Change::Range {
                    start_line: 1,
                    start_char: 2,
                    end_line: 3,
                    end_char: 4,
                    data: b"hello".to_vec()
                }
            }
        );
    }

    #[test]
    fn parses_json_open() {
        let cmd = parse_command(r#"["open","a.tex","x"]"#, WireProtocol::Json).unwrap();
        assert_eq!(
            cmd,
            EditorCommand::Open {
                path: "a.tex".to_string(),
                data: b"x".to_vec(),
                base64: false
            }
        );
    }

    #[test]
    fn serializes_lookup_file() {
        let msg = EditorMessage::LookupFile {
            kind: LookupKind::Read,
            status: LookupStatus::Promised,
            path: "missing.tex".to_string(),
        };
        assert_eq!(
            serialize_message(&msg, WireProtocol::Sexp),
            r#"(lookup-file read promised "missing.tex")"#
        );
        assert_eq!(
            serialize_message(&msg, WireProtocol::Json),
            r#"["lookup-file","read","promised","missing.tex"]"#
        );
    }

    #[test]
    fn serializes_append_with_buffer_offset_in_both_protocols() {
        // TeXpresso's byte-mode append carries the buffer write offset:
        // `(append <buffer> <pos> "<text>")`. A non-zero pos (partial streamed
        // log chunks) must be serialized in both protocols.
        let msg = EditorMessage::Append {
            buffer: InfoBuffer::Log,
            pos: 128,
            text: "rude output".to_string(),
        };
        assert_eq!(
            serialize_message(&msg, WireProtocol::Sexp),
            r#"(append log 128 "rude output")"#
        );
        assert_eq!(
            serialize_message(&msg, WireProtocol::Json),
            r#"["append","log",128,"rude output"]"#
        );
    }

    #[test]
    fn serializes_synctex_reverse_notification() {
        // The engine-to-editor click-to-source notification: the exact wire
        // string an editor parses to jump to a source location. A field-order
        // or quoting regression here would silently break every editor.
        let msg = EditorMessage::Synctex {
            path: "dir/main.tex".to_string(),
            line: 12,
            column: 3,
        };
        assert_eq!(
            serialize_message(&msg, WireProtocol::Sexp),
            r#"(synctex "dir/main.tex" 12 3)"#
        );
        assert_eq!(
            serialize_message(&msg, WireProtocol::Json),
            r#"["synctex","dir/main.tex",12,3]"#
        );
    }

    #[test]
    fn serializes_append_lines_flattened_in_both_protocols() {
        // Line-mode info-buffer append flattens each line into the message
        // array; the exact wire shape is what editors parse per line.
        let msg = EditorMessage::AppendLines {
            buffer: InfoBuffer::Out,
            lines: vec!["first".to_string(), "second".to_string()],
        };
        assert_eq!(
            serialize_message(&msg, WireProtocol::Sexp),
            r#"(append-lines out "first" "second")"#
        );
        assert_eq!(
            serialize_message(&msg, WireProtocol::Json),
            r#"["append-lines","out","first","second"]"#
        );
    }

    #[test]
    fn sexp_escapes_engine_text_specials() {
        // Engine log/stdout text is full of parens, quotes, backslashes and
        // newlines (TeX's per-char file-open echo, file paths, error messages).
        // The sexp escaper must quote it and escape the control chars while
        // leaving parens literal (they are harmless inside a quoted string).
        let msg = EditorMessage::Append {
            buffer: InfoBuffer::Out,
            pos: 0,
            text: "x\ty(z)\"w\\v\r\n".to_string(),
        };
        assert_eq!(
            serialize_message(&msg, WireProtocol::Sexp),
            r#"(append out 0 "x\ty(z)\"w\\v\r\n")"#
        );
    }

    #[test]
    fn sexp_parser_reads_escaped_text_with_parens() {
        // The other half of the wire: a quoted string containing an escaped
        // quote, an escaped backslash, a \n escape, and literal parens must
        // parse back to the exact bytes — critically, the inner `)` must NOT
        // terminate the enclosing list early.
        let cmd = parse_command(r#"(open "p.tex" "a\n(b)c\"d\\e")"#, WireProtocol::Sexp).unwrap();
        match cmd {
            EditorCommand::Open { path, data, base64 } => {
                assert_eq!(path, "p.tex");
                assert!(!base64);
                assert_eq!(String::from_utf8(data).unwrap(), "a\n(b)c\"d\\e");
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }
}
