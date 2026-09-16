use std::{fmt, io::Read};

use flate2::read::GzDecoder;
use oxipresso_engine_api::SyncTexArtifact;

pub type Result<T> = std::result::Result<T, SyncTexError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTexError {
    message: String,
}

impl SyncTexError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SyncTexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SyncTexError {}

impl From<std::io::Error> for SyncTexError {
    fn from(value: std::io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<std::string::FromUtf8Error> for SyncTexError {
    fn from(value: std::string::FromUtf8Error) -> Self {
        Self::new(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTexDocument {
    pub output: Option<String>,
    pub inputs: Vec<SyncTexInput>,
    pub records: Vec<SyncTexRecord>,
}

impl SyncTexDocument {
    pub fn input_by_index(&self, index: usize) -> Option<&SyncTexInput> {
        self.inputs.iter().find(|input| input.index == index)
    }

    pub fn forward_search_index(&self, input_index: usize, line: usize) -> Option<SyncTexHit> {
        let input = self.input_by_index(input_index)?;
        self.records
            .iter()
            .filter(|record| record.input_index == input_index)
            .min_by_key(|record| record.line.abs_diff(line))
            .map(|record| SyncTexHit {
                page: record.page,
                input_index,
                path: input.path.clone(),
                line: record.line,
                x: record.x,
                y: record.y,
                width: record.width,
                height: record.height,
                depth: record.depth,
            })
    }

    pub fn forward_search_path(&self, path: &str, line: usize) -> Option<SyncTexHit> {
        let input = self
            .inputs
            .iter()
            .find(|input| paths_match(&input.path, path))?;
        self.forward_search_index(input.index, line)
    }

    pub fn reverse_search_page_point(&self, page: usize, x: i32, y: i32) -> Option<SyncTexHit> {
        self.records
            .iter()
            .filter(|record| record.page == page)
            .filter_map(|record| {
                let input = self.input_by_index(record.input_index)?;
                Some((record, input, coordinate_distance_squared(record, x, y)))
            })
            .min_by_key(|(_, _, distance)| *distance)
            .map(|(record, input, _)| SyncTexHit {
                page: record.page,
                input_index: record.input_index,
                path: input.path.clone(),
                line: record.line,
                x: record.x,
                y: record.y,
                width: record.width,
                height: record.height,
                depth: record.depth,
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTexInput {
    pub index: usize,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTexRecord {
    pub kind: char,
    pub page: usize,
    pub input_index: usize,
    pub line: usize,
    pub x: i32,
    pub y: i32,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub depth: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTexHit {
    pub page: usize,
    pub input_index: usize,
    pub path: String,
    pub line: usize,
    pub x: i32,
    pub y: i32,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub depth: Option<i32>,
}

pub fn parse_artifact(artifact: &SyncTexArtifact) -> Result<SyncTexDocument> {
    parse_bytes(&artifact.bytes, artifact.compressed)
}

pub fn parse_bytes(bytes: &[u8], compressed: bool) -> Result<SyncTexDocument> {
    let bytes = if compressed {
        let mut decoded = Vec::new();
        GzDecoder::new(bytes).read_to_end(&mut decoded)?;
        decoded
    } else {
        bytes.to_vec()
    };
    parse_text(&String::from_utf8(bytes)?)
}

pub fn parse_text(text: &str) -> Result<SyncTexDocument> {
    let mut output = None;
    let mut inputs = Vec::new();
    let mut records = Vec::new();
    let mut current_page = None;

    for line in text.lines() {
        if let Some(value) = line.strip_prefix("Output:") {
            output = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("Input:") {
            let Some((index, path)) = value.split_once(':') else {
                return Err(SyncTexError::new("invalid SyncTeX input line"));
            };
            inputs.push(SyncTexInput {
                index: index
                    .parse()
                    .map_err(|_| SyncTexError::new("invalid SyncTeX input index"))?,
                path: path.to_string(),
            });
        } else if let Some(page) = parse_sheet_start(line)? {
            current_page = Some(page);
        } else if let Some(page) = current_page
            && let Some(record) = parse_record_line(line, page)?
        {
            records.push(record);
        }
    }

    Ok(SyncTexDocument {
        output,
        inputs,
        records,
    })
}

fn parse_sheet_start(line: &str) -> Result<Option<usize>> {
    let Some(value) = line.strip_prefix('{') else {
        return Ok(None);
    };
    Ok(Some(value.parse().map_err(|_| {
        SyncTexError::new("invalid SyncTeX sheet page")
    })?))
}

fn parse_record_line(line: &str, page: usize) -> Result<Option<SyncTexRecord>> {
    let Some(kind) = line.chars().next() else {
        return Ok(None);
    };
    if !matches!(kind, '[' | '(' | 'h' | 'v' | 'x' | 'g' | 'k' | '$') {
        return Ok(None);
    }
    let value = &line[kind.len_utf8()..];
    let Some((source, dimensions)) = value.split_once(':') else {
        return Ok(None);
    };
    let Some((input_index, line)) = source.split_once(',') else {
        return Ok(None);
    };
    let values = dimensions
        .split([',', ':'])
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<i32>()
                .map_err(|_| SyncTexError::new("invalid SyncTeX record coordinate"))
        })
        .collect::<Result<Vec<_>>>()?;
    if values.len() < 2 {
        return Ok(None);
    }
    Ok(Some(SyncTexRecord {
        kind,
        page,
        input_index: input_index
            .parse()
            .map_err(|_| SyncTexError::new("invalid SyncTeX record input index"))?,
        line: line
            .parse()
            .map_err(|_| SyncTexError::new("invalid SyncTeX record line"))?,
        x: values[0],
        y: values[1],
        width: values.get(2).copied(),
        height: values.get(3).copied(),
        depth: values.get(4).copied(),
    }))
}

fn paths_match(candidate: &str, query: &str) -> bool {
    let candidate = normalize_path(candidate);
    let query = normalize_path(query);
    candidate.eq_ignore_ascii_case(&query)
        || path_has_suffix(&candidate, &query)
        || path_has_suffix(&query, &candidate)
}

fn path_has_suffix(path: &str, suffix: &str) -> bool {
    if suffix.is_empty() || path.len() <= suffix.len() {
        return false;
    }
    path.to_ascii_lowercase()
        .strip_suffix(&suffix.to_ascii_lowercase())
        .is_some_and(|prefix| prefix.ends_with('/'))
}

fn normalize_path(path: &str) -> String {
    let mut path = path.replace('\\', "/");
    while let Some(stripped) = path.strip_prefix("./") {
        path = stripped.to_string();
    }
    while path.contains("//") {
        path = path.replace("//", "/");
    }
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

fn coordinate_distance_squared(record: &SyncTexRecord, x: i32, y: i32) -> i64 {
    let dx = i64::from(record.x) - i64::from(x);
    let dy = i64::from(record.y) - i64::from(y);
    dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    #[test]
    fn parses_plain_synctex_metadata() {
        let document = parse_text(
            "SyncTeX Version:1\nOutput:main.pdf\nInput:1:/root/main.tex\nInput:2:inc/test.tex\n",
        )
        .unwrap();
        assert_eq!(document.output.as_deref(), Some("main.pdf"));
        assert_eq!(
            document.inputs,
            vec![
                SyncTexInput {
                    index: 1,
                    path: "/root/main.tex".to_string()
                },
                SyncTexInput {
                    index: 2,
                    path: "inc/test.tex".to_string()
                }
            ]
        );
        assert_eq!(
            document.input_by_index(2).map(|input| input.path.as_str()),
            Some("inc/test.tex")
        );
    }

    #[test]
    fn parses_records_and_forward_searches_by_path_or_index() {
        let document = parse_text(
            "SyncTeX Version:1\n\
Output:main.pdf\n\
Input:1:c:/tmp/main.tex\n\
Content:\n\
{1\n\
[1,6:4736287,46220575:26673152,41484288,0\n\
(1,3:8799519,8865055:22609920,454820,7208\n\
g1,3:11380982,8865055\n\
}\n",
        )
        .unwrap();
        assert_eq!(document.records.len(), 3);
        assert_eq!(
            document.records[0],
            SyncTexRecord {
                kind: '[',
                page: 1,
                input_index: 1,
                line: 6,
                x: 4736287,
                y: 46220575,
                width: Some(26673152),
                height: Some(41484288),
                depth: Some(0),
            }
        );
        let hit = document.forward_search_path("main.tex", 4).unwrap();
        assert_eq!(hit.page, 1);
        assert_eq!(hit.input_index, 1);
        assert_eq!(hit.line, 3);
        assert_eq!(hit.path, "c:/tmp/main.tex");
        assert!(document.forward_search_index(1, 6).is_some());
    }

    #[test]
    fn reverse_searches_nearest_record_on_page() {
        let document = parse_text(
            "SyncTeX Version:1\n\
Output:main.pdf\n\
Input:1:main.tex\n\
Content:\n\
{1\n\
g1,10:100,100\n\
g1,20:400,400\n\
}\n",
        )
        .unwrap();
        let hit = document.reverse_search_page_point(1, 390, 410).unwrap();
        assert_eq!(hit.path, "main.tex");
        assert_eq!(hit.line, 20);
        assert_eq!(hit.x, 400);
        assert_eq!(hit.y, 400);
        assert!(document.reverse_search_page_point(2, 390, 410).is_none());
    }

    #[test]
    fn forward_search_path_matches_windows_and_relative_variants() {
        let document = parse_text(
            "SyncTeX Version:1\n\
Output:main.pdf\n\
Input:1:C:\\Users\\demo\\project\\main.tex\n\
Input:2:sections/intro.tex\n\
Content:\n\
{1\n\
g1,10:100,100\n\
g2,20:200,200\n\
}\n",
        )
        .unwrap();

        assert_eq!(
            document
                .forward_search_path("./project//main.tex", 10)
                .unwrap()
                .input_index,
            1
        );
        assert_eq!(
            document
                .forward_search_path("C:/USERS/demo/project/main.tex", 10)
                .unwrap()
                .input_index,
            1
        );
        assert_eq!(
            document
                .forward_search_path("C:/tmp/work/sections/intro.tex", 20)
                .unwrap()
                .input_index,
            2
        );
    }

    #[test]
    fn parses_compressed_synctex_artifact() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(b"SyncTeX Version:1\nOutput:main.pdf\nInput:1:main.tex\n")
            .unwrap();
        let artifact = SyncTexArtifact {
            bytes: encoder.finish().unwrap(),
            compressed: true,
            source_name: Some("main.synctex.gz".to_string()),
        };
        let document = parse_artifact(&artifact).unwrap();
        assert_eq!(document.output.as_deref(), Some("main.pdf"));
        assert_eq!(document.input_by_index(1).unwrap().path, "main.tex");
    }

    #[test]
    fn rejects_invalid_input_index() {
        assert!(parse_text("Input:not-number:main.tex\n").is_err());
    }
}
