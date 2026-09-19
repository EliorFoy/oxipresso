use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use base64::Engine as _;
use oxipresso_editor_protocol::{Change, EditorCommand, LookupKind, LookupStatus};
use oxipresso_engine_api::{
    EngineError, EngineIo, FileHandle, FileKind, FileResolver, OpenResult, PictureKey, Result,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeOutcome {
    pub path: String,
    pub changed_offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupEvent {
    pub kind: LookupKind,
    pub status: LookupStatus,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEvent {
    pub index: usize,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub disk_data: Option<Vec<u8>>,
    pub edit_data: Option<Vec<u8>>,
    pub saved_output: Vec<u8>,
    pub promised: bool,
    pub read_requested: bool,
    pub seen_offset: Option<usize>,
    pub picture_bounds: HashMap<PictureKey, [f32; 4]>,
}

impl FileEntry {
    fn new(path: String) -> Self {
        Self {
            path,
            disk_data: None,
            edit_data: None,
            saved_output: Vec::new(),
            promised: false,
            read_requested: false,
            seen_offset: None,
            picture_bounds: HashMap::new(),
        }
    }

    fn readable_data(&self) -> Option<&[u8]> {
        self.edit_data
            .as_deref()
            .or(self.disk_data.as_deref())
            .or_else(|| (!self.saved_output.is_empty()).then_some(self.saved_output.as_slice()))
    }
}

#[derive(Debug, Clone)]
struct OpenFile {
    path: String,
    write: bool,
}

#[derive(Default)]
pub struct VirtualFileSystem {
    files: HashMap<String, FileEntry>,
    handles: HashMap<FileHandle, OpenFile>,
    lookup_events: Vec<LookupEvent>,
    input_events: Vec<InputEvent>,
    input_indices: HashMap<String, usize>,
    disk_roots: Vec<PathBuf>,
    resolver: Option<Box<dyn FileResolver>>,
    next_handle: u32,
    next_input_index: usize,
}

impl std::fmt::Debug for VirtualFileSystem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VirtualFileSystem")
            .field("files", &self.files.keys().collect::<Vec<_>>())
            .field("disk_roots", &self.disk_roots)
            .field("resolver", &self.resolver.as_ref().map(|_| "installed"))
            .field("next_handle", &self.next_handle)
            .field("next_input_index", &self.next_input_index)
            .finish()
    }
}

impl VirtualFileSystem {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
            handles: HashMap::new(),
            lookup_events: Vec::new(),
            input_events: Vec::new(),
            input_indices: HashMap::new(),
            disk_roots: Vec::new(),
            resolver: None,
            next_handle: 1,
            next_input_index: 0,
        }
    }

    pub fn normalize_path(path: impl AsRef<str>) -> String {
        let mut path = path.as_ref().replace('\\', "/");
        while let Some(stripped) = path.strip_prefix("./") {
            path = stripped.to_string();
        }
        while path.contains("//") {
            path = path.replace("//", "/");
        }
        path
    }

    pub fn lookup(&self, path: &str) -> Option<&FileEntry> {
        self.files.get(&Self::normalize_path(path))
    }

    pub fn lookup_mut(&mut self, path: &str) -> Option<&mut FileEntry> {
        self.files.get_mut(&Self::normalize_path(path))
    }

    pub fn ensure_file(&mut self, path: &str) -> &mut FileEntry {
        let path = Self::normalize_path(path);
        self.files
            .entry(path.clone())
            .or_insert_with(|| FileEntry::new(path))
    }

    pub fn take_lookup_events(&mut self) -> Vec<LookupEvent> {
        std::mem::take(&mut self.lookup_events)
    }

    pub fn take_input_events(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.input_events)
    }

    pub fn set_disk_roots<I, P>(&mut self, roots: I)
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.disk_roots = roots.into_iter().map(Into::into).collect();
    }

    pub fn set_resolver(&mut self, resolver: Box<dyn FileResolver>) {
        self.resolver = Some(resolver);
    }

    pub fn disk_roots(&self) -> &[PathBuf] {
        &self.disk_roots
    }

    pub fn load_disk_file(&mut self, path: impl AsRef<Path>) -> std::io::Result<ChangeOutcome> {
        let path_ref = path.as_ref();
        let data = std::fs::read(path_ref)?;
        let key = Self::normalize_path(path_ref.to_string_lossy());
        let entry = self.ensure_file(&key);
        let changed_offset = entry
            .disk_data
            .as_deref()
            .map(|old| first_diff(old, &data))
            .unwrap_or(Some(0));
        entry.disk_data = Some(data);
        Ok(ChangeOutcome {
            path: key,
            changed_offset,
        })
    }

    pub fn apply_editor_command(
        &mut self,
        command: &EditorCommand,
    ) -> Result<Option<ChangeOutcome>> {
        match command {
            EditorCommand::Open { path, data, base64 } => {
                let data = if *base64 {
                    base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(|e| EngineError::new(format!("invalid base64 payload: {e}")))?
                } else {
                    data.clone()
                };
                Ok(Some(self.open_editor(path, data)))
            }
            EditorCommand::Close { path } => Ok(self.close_editor(path)),
            EditorCommand::Change { path, change } => self.apply_change(path, change).map(Some),
            EditorCommand::Register { path } => {
                self.ensure_file(path).promised = true;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    pub fn open_editor(&mut self, path: &str, data: Vec<u8>) -> ChangeOutcome {
        let key = Self::normalize_path(path);
        let entry = self.ensure_file(&key);
        let changed_offset = entry
            .edit_data
            .as_deref()
            .or(entry.disk_data.as_deref())
            .map(|old| first_diff(old, &data))
            .unwrap_or_else(|| {
                (entry.promised || entry.read_requested || entry.seen_offset.is_some()).then_some(0)
            });
        entry.edit_data = Some(data);
        entry.promised = false;
        ChangeOutcome {
            path: key,
            changed_offset,
        }
    }

    pub fn close_editor(&mut self, path: &str) -> Option<ChangeOutcome> {
        let key = Self::normalize_path(path);
        let entry = self.files.get_mut(&key)?;
        let edit_data = entry.edit_data.take()?;
        let changed_offset = entry
            .disk_data
            .as_deref()
            .map(|disk| first_diff(disk, &edit_data))
            .unwrap_or(Some(0));
        Some(ChangeOutcome {
            path: key,
            changed_offset,
        })
    }

    pub fn apply_change(&mut self, path: &str, change: &Change) -> Result<ChangeOutcome> {
        let key = Self::normalize_path(path);
        let entry = self
            .files
            .get_mut(&key)
            .ok_or_else(|| EngineError::new(format!("{key}: file is not open")))?;
        let data = entry
            .edit_data
            .as_mut()
            .ok_or_else(|| EngineError::new(format!("{key}: file is not open in editor VFS")))?;
        let (offset, remove, insert) = match change {
            Change::Bytes {
                offset,
                remove,
                data,
            } => (*offset, *remove, data.as_slice()),
            Change::Lines {
                offset,
                remove,
                data: insert,
            } => {
                let start = byte_offset_for_line(data, *offset)
                    .ok_or_else(|| EngineError::new("invalid line offset"))?;
                let end = byte_offset_after_lines(data, start, *remove)
                    .ok_or_else(|| EngineError::new("invalid line count"))?;
                (start, end - start, insert.as_slice())
            }
            Change::Range {
                start_line,
                start_char,
                end_line,
                end_char,
                data: insert,
            } => {
                let start_line_offset = byte_offset_for_line(data, *start_line)
                    .ok_or_else(|| EngineError::new("invalid start line"))?;
                let start = start_line_offset
                    + utf16_to_utf8_offset(&data[start_line_offset..], *start_char)
                        .ok_or_else(|| EngineError::new("invalid start UTF-16 column"))?;
                let end_line_offset = byte_offset_for_line(data, *end_line)
                    .ok_or_else(|| EngineError::new("invalid end line"))?;
                let end = end_line_offset
                    + utf16_to_utf8_offset(&data[end_line_offset..], *end_char)
                        .ok_or_else(|| EngineError::new("invalid end UTF-16 column"))?;
                if end < start {
                    return Err(EngineError::new("range end precedes start"));
                }
                (start, end - start, insert.as_slice())
            }
        };
        // Editor-supplied offset/remove are f64->usize casts and can be huge, so
        // `offset + remove` may overflow (debug panic / release wrap into a
        // reversed slice). Use checked arithmetic: an overflow means the range
        // is past the file, matching TeXpresso's bounded skip (a catchable Err,
        // not a crash). Valid inputs are unaffected.
        let end = offset
            .checked_add(remove)
            .ok_or_else(|| EngineError::new("change range is outside the file"))?;
        if end > data.len() {
            return Err(EngineError::new("change range is outside the file"));
        }
        data.splice(offset..end, insert.iter().copied());
        Ok(ChangeOutcome {
            path: key,
            changed_offset: Some(offset),
        })
    }

    fn alloc_handle(&mut self, path: String, write: bool) -> FileHandle {
        let handle = FileHandle(self.next_handle);
        self.next_handle += 1;
        self.handles.insert(handle, OpenFile { path, write });
        handle
    }

    fn note_input_file(&mut self, path: &str) {
        if self.input_indices.contains_key(path) {
            return;
        }
        let index = self.next_input_index;
        self.next_input_index += 1;
        self.input_indices.insert(path.to_string(), index);
        self.input_events.push(InputEvent {
            index,
            path: path.to_string(),
        });
    }

    fn resolve_external(&mut self, key: &str, kind: FileKind) -> Result<Option<Vec<u8>>> {
        if let Some(resolver) = self.resolver.as_mut()
            && let Some(data) = resolver.resolve(key, kind)
        {
            return Ok(Some(data));
        }
        Ok(None)
    }

    fn read_disk_candidate(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let normalized = PathBuf::from(key.replace('/', std::path::MAIN_SEPARATOR_STR));
        if normalized.is_absolute() {
            if normalized.is_file() {
                return Ok(Some(std::fs::read(normalized)?));
            }
            return Ok(None);
        }

        for root in &self.disk_roots {
            let candidate = root.join(&normalized);
            if candidate.is_file() {
                return Ok(Some(std::fs::read(candidate)?));
            }
        }
        Ok(None)
    }
}

impl EngineIo for VirtualFileSystem {
    fn open_read(&mut self, path: &str, kind: FileKind) -> Result<OpenResult> {
        let key = Self::normalize_path(path);
        self.ensure_file(&key).read_requested = true;
        if self
            .files
            .get(&key)
            .and_then(FileEntry::readable_data)
            .is_some()
        {
            let handle = self.alloc_handle(key.clone(), false);
            self.lookup_events.push(LookupEvent {
                kind: LookupKind::Read,
                status: LookupStatus::Successful,
                path: key.clone(),
            });
            self.note_input_file(&key);
            Ok(OpenResult::Opened {
                handle,
                canonical_path: key,
            })
        } else if self.files.get(&key).is_some_and(|entry| entry.promised) {
            self.lookup_events.push(LookupEvent {
                kind: LookupKind::Read,
                status: LookupStatus::Promised,
                path: key,
            });
            Ok(OpenResult::Promised)
        } else if let Some(data) = self.read_disk_candidate(&key)? {
            self.ensure_file(&key).disk_data = Some(data);
            let handle = self.alloc_handle(key.clone(), false);
            self.lookup_events.push(LookupEvent {
                kind: LookupKind::Read,
                status: LookupStatus::Successful,
                path: key.clone(),
            });
            self.note_input_file(&key);
            Ok(OpenResult::Opened {
                handle,
                canonical_path: key,
            })
        } else if let Some(data) = self.resolve_external(&key, kind)? {
            self.ensure_file(&key).disk_data = Some(data);
            let handle = self.alloc_handle(key.clone(), false);
            self.lookup_events.push(LookupEvent {
                kind: LookupKind::Read,
                status: LookupStatus::Successful,
                path: key.clone(),
            });
            self.note_input_file(&key);
            Ok(OpenResult::Opened {
                handle,
                canonical_path: key,
            })
        } else {
            self.ensure_file(&key);
            self.lookup_events.push(LookupEvent {
                kind: LookupKind::Read,
                status: LookupStatus::Failed,
                path: key,
            });
            Ok(OpenResult::Missing)
        }
    }

    fn open_write(&mut self, path: &str, _kind: FileKind) -> Result<FileHandle> {
        let key = Self::normalize_path(path);
        self.ensure_file(&key).saved_output.clear();
        self.lookup_events.push(LookupEvent {
            kind: LookupKind::Write,
            status: LookupStatus::Successful,
            path: key.clone(),
        });
        Ok(self.alloc_handle(key, true))
    }

    fn read(&mut self, handle: FileHandle, offset: usize, len: usize) -> Result<Vec<u8>> {
        let open = self
            .handles
            .get(&handle)
            .ok_or_else(|| EngineError::new("invalid file handle"))?;
        if open.write {
            return Err(EngineError::new("cannot read from write handle"));
        }
        let entry = self
            .files
            .get(&open.path)
            .ok_or_else(|| EngineError::new("handle points to missing file"))?;
        let data = entry
            .readable_data()
            .ok_or_else(|| EngineError::new("file has no readable data"))?;
        if offset > data.len() {
            return Err(EngineError::new("read offset is outside the file"));
        }
        // `offset + len` can overflow for a huge requested length; saturate so a
        // large read clamps to the end of the file instead of panicking. Valid
        // lengths are unaffected.
        let end = offset.saturating_add(len).min(data.len());
        Ok(data[offset..end].to_vec())
    }

    fn size(&mut self, handle: FileHandle) -> Result<usize> {
        let open = self
            .handles
            .get(&handle)
            .ok_or_else(|| EngineError::new("invalid file handle"))?;
        let entry = self
            .files
            .get(&open.path)
            .ok_or_else(|| EngineError::new("handle points to missing file"))?;
        if open.write {
            Ok(entry.saved_output.len())
        } else {
            entry
                .readable_data()
                .map(|data| data.len())
                .ok_or_else(|| EngineError::new("file has no readable data"))
        }
    }

    fn append(&mut self, handle: FileHandle, bytes: &[u8]) -> Result<()> {
        let open = self
            .handles
            .get(&handle)
            .ok_or_else(|| EngineError::new("invalid file handle"))?;
        if !open.write {
            return Err(EngineError::new("cannot append to read handle"));
        }
        self.files
            .get_mut(&open.path)
            .ok_or_else(|| EngineError::new("handle points to missing file"))?
            .saved_output
            .extend_from_slice(bytes);
        Ok(())
    }

    fn seen(&mut self, handle: FileHandle, offset: usize, _engine_time: u64) {
        if let Some(open) = self.handles.get(&handle)
            && let Some(entry) = self.files.get_mut(&open.path)
        {
            entry.seen_offset = Some(entry.seen_offset.unwrap_or(0).max(offset));
        }
    }

    fn close(&mut self, handle: FileHandle) -> Result<()> {
        self.handles
            .remove(&handle)
            .ok_or_else(|| EngineError::new("invalid file handle"))?;
        Ok(())
    }

    fn picture_bounds_get(&mut self, key: &PictureKey) -> Option<[f32; 4]> {
        self.files
            .get(&Self::normalize_path(&key.path))
            .and_then(|entry| entry.picture_bounds.get(key).copied())
    }

    fn picture_bounds_set(&mut self, key: PictureKey, bounds: [f32; 4]) {
        self.ensure_file(&key.path)
            .picture_bounds
            .insert(key, bounds);
    }

    fn snapshot_inputs(&mut self) -> Result<Vec<(String, Vec<u8>)>> {
        Ok(self
            .files
            .values()
            .filter_map(|entry| {
                entry
                    .edit_data
                    .as_deref()
                    .or(entry.disk_data.as_deref())
                    .map(|data| (entry.path.clone(), data.to_vec()))
            })
            .collect())
    }

    fn inject_editor(&mut self, path: &str, bytes: Vec<u8>) -> bool {
        // Editor-channel injection used by the checkpoint fence: identical to
        // an `open` command arriving while the engine is parked.
        let _outcome = self.open_editor(path, bytes);
        true
    }
}

fn first_diff(old: &[u8], new: &[u8]) -> Option<usize> {
    let len = old.len().min(new.len());
    for index in 0..len {
        if old[index] != new[index] {
            return Some(index);
        }
    }
    (old.len() != new.len()).then_some(len)
}

fn byte_offset_for_line(data: &[u8], line: usize) -> Option<usize> {
    let mut offset = 0;
    let mut remaining = line;
    while remaining > 0 {
        if offset >= data.len() {
            return None;
        }
        if data[offset] == b'\n' {
            remaining -= 1;
        }
        offset += 1;
    }
    Some(offset)
}

fn byte_offset_after_lines(data: &[u8], start: usize, count: usize) -> Option<usize> {
    let mut offset = start;
    let mut remaining = count;
    while remaining > 0 {
        if offset >= data.len() {
            return (remaining == 1).then_some(data.len());
        }
        if data[offset] == b'\n' {
            remaining -= 1;
        }
        offset += 1;
    }
    Some(offset)
}

fn utf16_to_utf8_offset(data: &[u8], utf16_units: usize) -> Option<usize> {
    let text = std::str::from_utf8(data).ok()?;
    let mut units = 0;
    for (byte_offset, ch) in text.char_indices() {
        if units == utf16_units {
            return Some(byte_offset);
        }
        units += ch.len_utf16();
        if units > utf16_units {
            return None;
        }
    }
    (units == utf16_units).then_some(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fuzzer(u64);
    impl Fuzzer {
        fn n(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6F_DD1D)
        }
    }
    fn huge_idx(f: &mut Fuzzer, len: usize) -> usize {
        match f.n() % 6 {
            0 => (f.n() % (len as u64 + 2)) as usize,
            1 => usize::MAX,
            2 => usize::MAX / 2,
            3 => len,
            4 => len.saturating_add(1),
            _ => (f.n() % 8) as usize,
        }
    }

    #[test]
    fn fuzz_apply_change_never_panics() {
        // Editor offsets/line counts/UTF-16 columns arrive as f64->usize casts and
        // can be arbitrarily large; and VFS bytes may be non-UTF-8. apply_change
        // must reject every bad change with Err, never panic (splice/index/
        // offset-underflow/UTF-16-column conversion). Fresh VFS per case.
        let mut f = Fuzzer(0x2545_F491_4F6F_DD1D ^ 0xC0FFEE);
        for _case in 0..8000usize {
            let clen = (f.n() % 40) as usize;
            let mut content = Vec::with_capacity(clen);
            for _ in 0..clen {
                content.push(match f.n() % 4 {
                    0 => b'\n',
                    1 => b'a',
                    2 => 0xE4,
                    _ => b' ',
                });
            }
            let mut vfs = VirtualFileSystem::new();
            vfs.open_editor("m.tex", content);
            let ilen = (f.n() % 6) as usize;
            let mut ins = vec![0u8; ilen];
            for b in &mut ins {
                *b = (f.n() % 256) as u8;
            }
            let change = match f.n() % 3 {
                0 => Change::Bytes {
                    offset: huge_idx(&mut f, clen),
                    remove: huge_idx(&mut f, clen),
                    data: ins,
                },
                1 => Change::Lines {
                    offset: huge_idx(&mut f, clen),
                    remove: huge_idx(&mut f, clen),
                    data: ins,
                },
                _ => Change::Range {
                    start_line: huge_idx(&mut f, clen),
                    start_char: huge_idx(&mut f, clen),
                    end_line: huge_idx(&mut f, clen),
                    end_char: huge_idx(&mut f, clen),
                    data: ins,
                },
            };
            let _ = vfs.apply_change("m.tex", &change);
        }
        // Control: a valid change still applies (guards a blanket-reject parser).
        let mut v = VirtualFileSystem::new();
        v.open_editor("m.tex", b"hello world".to_vec());
        assert!(
            v.apply_change(
                "m.tex",
                &Change::Bytes {
                    offset: 6,
                    remove: 5,
                    data: b"tex".to_vec()
                },
            )
            .is_ok()
        );
    }

    #[test]
    fn applies_byte_change() {
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("main.tex", b"hello world".to_vec());
        let outcome = vfs
            .apply_change(
                "main.tex",
                &Change::Bytes {
                    offset: 6,
                    remove: 5,
                    data: b"tex".to_vec(),
                },
            )
            .unwrap();
        assert_eq!(outcome.changed_offset, Some(6));
        assert_eq!(
            vfs.lookup("main.tex").unwrap().edit_data.as_deref(),
            Some(&b"hello tex"[..])
        );
    }

    #[test]
    fn applies_line_change() {
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("main.tex", b"a\nb\nc\n".to_vec());
        vfs.apply_change(
            "main.tex",
            &Change::Lines {
                offset: 1,
                remove: 1,
                data: b"B\n".to_vec(),
            },
        )
        .unwrap();
        assert_eq!(
            vfs.lookup("main.tex").unwrap().edit_data.as_deref(),
            Some(&b"a\nB\nc\n"[..])
        );
    }

    #[test]
    fn change_lines_matches_texpresso_line_semantics() {
        // Source-verified against TeXpresso main.c (BASE_LINE): line offsets are
        // 0-based newline counts; `remove` consumes that many newlines
        // inclusive; dropping past an unterminated final line is tolerated
        // (clamps to EOF) but two or more short is an error.
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("m.tex", b"a\nb\nc\n".to_vec());
        vfs.apply_change(
            "m.tex",
            &Change::Lines {
                offset: 0,
                remove: 2,
                data: b"X\n".to_vec(),
            },
        )
        .unwrap();
        assert_eq!(
            vfs.lookup("m.tex").unwrap().edit_data.as_deref(),
            Some(&b"X\nc\n"[..]),
            "multi-line replace consumes each line's newline"
        );

        let mut unterminated = VirtualFileSystem::new();
        unterminated.open_editor("m.tex", b"a\nb".to_vec());
        unterminated
            .apply_change(
                "m.tex",
                &Change::Lines {
                    offset: 1,
                    remove: 1,
                    data: b"B".to_vec(),
                },
            )
            .unwrap();
        assert_eq!(
            unterminated.lookup("m.tex").unwrap().edit_data.as_deref(),
            Some(&b"a\nB"[..]),
            "removing 1 unterminated final line clamps to EOF (tolerated)"
        );

        let mut too_many = VirtualFileSystem::new();
        too_many.open_editor("m.tex", b"a\nb".to_vec());
        assert!(
            too_many
                .apply_change(
                    "m.tex",
                    &Change::Lines {
                        offset: 1,
                        remove: 2,
                        data: b"B".to_vec(),
                    },
                )
                .is_err(),
            "remove two short of an unterminated tail is an error"
        );

        let mut bad_offset = VirtualFileSystem::new();
        bad_offset.open_editor("m.tex", b"a\n".to_vec());
        assert!(
            bad_offset
                .apply_change(
                    "m.tex",
                    &Change::Lines {
                        offset: 5,
                        remove: 1,
                        data: b"x".to_vec(),
                    },
                )
                .is_err(),
            "line offset past the last line is an error"
        );
    }

    #[test]
    fn huge_line_and_column_numbers_are_rejected_without_hang_or_panic() {
        // change-lines/change-range derive byte spans by scanning for newlines /
        // UTF-16 columns. A scanner that looped `while remaining > 0` on an
        // editor-supplied usize::MAX line or column would hang; the bounded
        // scanners must instead scan the buffer once and return an error. An
        // unbounded implementation stalls this test (i.e. the test IS the guard).
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("lines.tex", b"a\nb\nc".to_vec());
        assert!(
            vfs.apply_change(
                "lines.tex",
                &Change::Lines {
                    offset: usize::MAX,
                    remove: usize::MAX,
                    data: vec![b'x'],
                }
            )
            .is_err(),
            "huge start line is an Err, not a hang/panic"
        );
        assert!(
            vfs.apply_change(
                "lines.tex",
                &Change::Range {
                    start_line: 0,
                    start_char: usize::MAX, // column past any real line
                    end_line: 2,
                    end_char: 1,
                    data: vec![b'x'],
                }
            )
            .is_err(),
            "huge UTF-16 column is an Err, not a hang/panic"
        );
        // The buffer is unchanged and still usable afterwards.
        assert_eq!(
            vfs.lookup("lines.tex")
                .unwrap()
                .edit_data
                .as_deref()
                .unwrap(),
            b"a\nb\nc"
        );
    }

    #[test]
    fn huge_change_offsets_are_rejected_without_overflow_panic() {
        // offset/remove are f64->usize casts and can be enormous; `offset +
        // remove` must be computed with checked arithmetic so it cannot overflow
        // the bounds check (a debug panic / release reversed slice). In-range
        // edits are unaffected.
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("big.tex", b"hello".to_vec());
        let err = vfs
            .apply_change(
                "big.tex",
                &Change::Bytes {
                    offset: usize::MAX,
                    remove: 1,
                    data: vec![b'x'],
                },
            )
            .expect_err("offset+remove overflow must be an Err, not a panic");
        assert!(err.to_string().contains("outside the file"));
        // A valid full-content edit still applies.
        vfs.apply_change(
            "big.tex",
            &Change::Bytes {
                offset: 0,
                remove: 5,
                data: b"hi".to_vec(),
            },
        )
        .unwrap();
        assert_eq!(
            vfs.lookup("big.tex").unwrap().edit_data.as_deref().unwrap(),
            b"hi"
        );
    }

    #[test]
    fn change_range_spans_multiple_lines_with_utf16_columns() {
        // Source-faithful to TeXpresso main.c BASE_RANGE: columns are UTF-16 code
        // units relative to each endpoint's line start, and start_line..end_line
        // may differ. Here [line0 char5 .. line1 char1] spans "\nw".
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("m.tex", "hello\nw😀rld\n".as_bytes().to_vec());
        vfs.apply_change(
            "m.tex",
            &Change::Range {
                start_line: 0,
                start_char: 5,
                end_line: 1,
                end_char: 1, // one UTF-16 unit past the line-1 'w'
                data: "|".as_bytes().to_vec(),
            },
        )
        .unwrap();
        assert_eq!(
            std::str::from_utf8(vfs.lookup("m.tex").unwrap().edit_data.as_deref().unwrap())
                .unwrap(),
            "hello|😀rld\n",
            "multi-line range replaces bytes [5..7) = newline + 'w'"
        );
    }

    #[test]
    fn applies_utf16_range_change() {
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("main.tex", "a\n😀b\n".as_bytes().to_vec());
        vfs.apply_change(
            "main.tex",
            &Change::Range {
                start_line: 1,
                start_char: 2,
                end_line: 1,
                end_char: 3,
                data: b"B".to_vec(),
            },
        )
        .unwrap();
        assert_eq!(
            std::str::from_utf8(
                vfs.lookup("main.tex")
                    .unwrap()
                    .edit_data
                    .as_deref()
                    .unwrap()
            )
            .unwrap(),
            "a\n😀B\n"
        );
    }

    #[test]
    fn utf16_column_mapping_handles_surrogate_boundaries() {
        // "a😀b": UTF-16 columns map to byte offsets, counting the astral char
        // as its 2 surrogate units. A column landing INSIDE the surrogate pair
        // (col 2) is rejected, not silently snapped to a byte boundary — the
        // existing range test only exercises the valid (non-splitting) path.
        let data = "a😀b".as_bytes(); // a@0, emoji@1..4 (4 bytes), b@5, len 6
        assert_eq!(utf16_to_utf8_offset(data, 0), Some(0));
        assert_eq!(utf16_to_utf8_offset(data, 1), Some(1), "start of emoji");
        assert_eq!(
            utf16_to_utf8_offset(data, 2),
            None,
            "mid-surrogate rejected"
        );
        assert_eq!(utf16_to_utf8_offset(data, 3), Some(5), "start of b");
        assert_eq!(utf16_to_utf8_offset(data, 4), Some(6), "end of string");
        assert_eq!(utf16_to_utf8_offset(data, 5), None, "beyond end");
    }

    #[test]
    fn promised_open_reports_promised() {
        let mut vfs = VirtualFileSystem::new();
        vfs.ensure_file("missing.tex").promised = true;
        assert_eq!(
            vfs.open_read("missing.tex", FileKind::Tex).unwrap(),
            OpenResult::Promised
        );
        assert_eq!(
            vfs.take_lookup_events(),
            vec![LookupEvent {
                kind: LookupKind::Read,
                status: LookupStatus::Promised,
                path: "missing.tex".to_string()
            }]
        );
    }

    #[test]
    fn opening_promised_file_reports_change_from_start() {
        let mut vfs = VirtualFileSystem::new();
        vfs.ensure_file("missing.tex").promised = true;
        let outcome = vfs.open_editor("missing.tex", b"now available".to_vec());
        assert_eq!(outcome.changed_offset, Some(0));
    }

    #[test]
    fn opening_failed_lookup_file_reports_change_from_start() {
        let mut vfs = VirtualFileSystem::new();
        assert_eq!(
            vfs.open_read("missing.tex", FileKind::Tex).unwrap(),
            OpenResult::Missing
        );
        let outcome = vfs.open_editor("missing.tex", b"now available".to_vec());
        assert_eq!(outcome.changed_offset, Some(0));
    }

    #[test]
    fn open_read_falls_back_to_disk_roots() {
        let temp = unique_temp_dir();
        let inc = temp.join("inc");
        std::fs::create_dir_all(&inc).unwrap();
        std::fs::write(inc.join("included.tex"), b"from disk").unwrap();

        let mut vfs = VirtualFileSystem::new();
        vfs.set_disk_roots([inc]);
        let opened = vfs.open_read("included.tex", FileKind::Tex).unwrap();
        let OpenResult::Opened { handle, .. } = opened else {
            panic!("expected disk-backed open");
        };
        assert_eq!(vfs.read(handle, 0, 128).unwrap(), b"from disk");
        assert_eq!(
            vfs.take_lookup_events(),
            vec![LookupEvent {
                kind: LookupKind::Read,
                status: LookupStatus::Successful,
                path: "included.tex".to_string()
            }]
        );
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn open_editor_data_overrides_disk_root_data() {
        let temp = unique_temp_dir();
        std::fs::create_dir_all(&temp).unwrap();
        std::fs::write(temp.join("main.tex"), b"from disk").unwrap();

        let mut vfs = VirtualFileSystem::new();
        vfs.set_disk_roots([temp.clone()]);
        vfs.open_editor("main.tex", b"from editor".to_vec());
        let OpenResult::Opened { handle, .. } = vfs.open_read("main.tex", FileKind::Tex).unwrap()
        else {
            panic!("expected editor-backed open");
        };
        assert_eq!(vfs.read(handle, 0, 128).unwrap(), b"from editor");
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn read_with_huge_len_saturates_without_overflow_panic() {
        // A backend requesting an enormous read length must not overflow the
        // `offset + len` slice bound (offset > EOF is already guarded). Valid
        // lengths are unaffected; a huge one clamps to the end of the file.
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("main.tex", b"abcde".to_vec());
        let OpenResult::Opened { handle, .. } = vfs.open_read("main.tex", FileKind::Tex).unwrap()
        else {
            panic!("expected editor-backed open");
        };
        assert_eq!(vfs.read(handle, 1, usize::MAX).unwrap(), b"bcde");
        // offset past EOF stays a guarded Err (start never exceeds end).
        assert!(vfs.read(handle, 999, 5).is_err());
    }

    #[test]
    fn snapshot_inputs_exports_editor_and_disk_inputs() {
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("main.tex", b"editor root".to_vec());
        vfs.ensure_file("disk.tex").disk_data = Some(b"disk include".to_vec());
        let handle = vfs.open_write("stdout", FileKind::Other).unwrap();
        vfs.append(handle, b"engine output").unwrap();

        let mut snapshot = vfs.snapshot_inputs().unwrap();
        snapshot.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            snapshot,
            vec![
                ("disk.tex".to_string(), b"disk include".to_vec()),
                ("main.tex".to_string(), b"editor root".to_vec()),
            ]
        );
    }

    #[test]
    fn open_base64_command_decodes_payload_or_errors() {
        // `open-base64` must decode the standard-alphabet payload into the
        // editor buffer, and must reject malformed base64 rather than store a
        // silently-wrong decode.
        let mut vfs = VirtualFileSystem::new();
        let good = EditorCommand::Open {
            path: "b.tex".to_string(),
            data: b"aGVsbG8=".to_vec(), // base64("hello")
            base64: true,
        };
        vfs.apply_editor_command(&good).unwrap();
        assert_eq!(
            vfs.lookup("b.tex").and_then(|e| e.edit_data.clone()),
            Some(b"hello".to_vec())
        );

        let bad = EditorCommand::Open {
            path: "c.tex".to_string(),
            data: b"!!!not base64!!!".to_vec(),
            base64: true,
        };
        assert!(
            vfs.apply_editor_command(&bad).is_err(),
            "malformed base64 must be rejected"
        );
    }

    #[test]
    fn close_editor_reverts_to_disk_and_reports_change_once() {
        // Editors expect: closing a buffer reverts reads to the on-disk bytes,
        // reports a change (so a rebuild can drop the edit), and a second close
        // or closing a never-edited file is a no-op returning None.
        let mut vfs = VirtualFileSystem::new();
        vfs.ensure_file("a.tex").disk_data = Some(b"disk version".to_vec());
        vfs.open_editor("a.tex", b"editor version".to_vec());
        assert_eq!(
            vfs.lookup("a.tex").unwrap().edit_data.as_deref(),
            Some(&b"editor version"[..])
        );

        let outcome = vfs
            .close_editor("a.tex")
            .expect("closing an edited buffer reports");
        assert_eq!(
            outcome.changed_offset,
            Some(0),
            "diff starts at byte 0 (d/e)"
        );
        let entry = vfs.lookup("a.tex").unwrap();
        assert!(entry.edit_data.is_none(), "editor buffer is dropped");
        assert_eq!(entry.disk_data.as_deref(), Some(&b"disk version"[..]));

        // Second close: no edit_data left => None (no spurious change).
        assert!(vfs.close_editor("a.tex").is_none());
        // Unknown path => None.
        assert!(vfs.close_editor("nope.tex").is_none());
    }

    #[test]
    fn close_editor_without_disk_reports_zero_offset() {
        // TeXpresso interpret_close: with no fs_data, `changed` stays 0 (still
        // reports a change so the engine reloads / sees the now-absent file).
        // Exercises the unwrap_or(Some(0)) branch, distinct from the with-disk
        // first_diff path covered above.
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("only-editor.tex", b"hello".to_vec()); // editor-only, no disk
        let outcome = vfs
            .close_editor("only-editor.tex")
            .expect("closing an editor-only buffer still reports a change");
        assert_eq!(outcome.changed_offset, Some(0));
    }

    #[test]
    fn open_read_records_input_file_once() {
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("main.tex", b"content".to_vec());
        let first = vfs.open_read("main.tex", FileKind::Tex).unwrap();
        let OpenResult::Opened { handle, .. } = first else {
            panic!("expected opened file");
        };
        vfs.close(handle).unwrap();
        let second = vfs.open_read("main.tex", FileKind::Tex).unwrap();
        let OpenResult::Opened { handle, .. } = second else {
            panic!("expected opened file");
        };
        vfs.close(handle).unwrap();

        assert_eq!(
            vfs.take_input_events(),
            vec![InputEvent {
                index: 0,
                path: "main.tex".to_string()
            }]
        );
    }

    #[test]
    fn input_indices_are_stable_across_rebuilds() {
        // The editor maps input-file index <-> path; indices must be assigned
        // once and stay fixed for the VFS lifetime, so a rebuild (which reopens
        // the same files) neither re-emits nor renumbers them, and a newly read
        // file takes the next monotonic index.
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("main.tex", b"a".to_vec());
        vfs.open_editor("inc.tex", b"b".to_vec());
        let open = |vfs: &mut VirtualFileSystem, path: &str| {
            if let OpenResult::Opened { handle, .. } = vfs.open_read(path, FileKind::Tex).unwrap() {
                vfs.close(handle).unwrap();
            }
        };
        open(&mut vfs, "main.tex");
        open(&mut vfs, "inc.tex");
        let initial = vfs.take_input_events();
        assert_eq!(
            initial,
            vec![
                InputEvent {
                    index: 0,
                    path: "main.tex".to_string()
                },
                InputEvent {
                    index: 1,
                    path: "inc.tex".to_string()
                },
            ]
        );
        // Rebuild: reopening existing inputs emits nothing and renumbers nothing.
        open(&mut vfs, "main.tex");
        open(&mut vfs, "inc.tex");
        assert!(
            vfs.take_input_events().is_empty(),
            "reopening existing input files must not re-emit or renumber indices"
        );
        // A newly read file continues the monotonic sequence; existing stay.
        vfs.open_editor("new.tex", b"c".to_vec());
        open(&mut vfs, "new.tex");
        assert_eq!(
            vfs.take_input_events(),
            vec![InputEvent {
                index: 2,
                path: "new.tex".to_string()
            }]
        );
    }

    #[test]
    fn first_diff_reports_first_diverging_byte() {
        // `first_diff` computes `changed_offset` for editor-open/disk-revert,
        // which drives rebuild-skip; a wrong offset would mis-target rebuilds.
        assert_eq!(first_diff(b"abc", b"abc"), None, "identical -> no change");
        assert_eq!(first_diff(b"abc", b"abd"), Some(2), "mid mismatch");
        assert_eq!(first_diff(b"abc", b"xbc"), Some(0), "first byte differs");
        assert_eq!(first_diff(b"abc", b"abcd"), Some(3), "append -> old len");
        assert_eq!(first_diff(b"abcd", b"abc"), Some(3), "trim -> shorter len");
        assert_eq!(first_diff(b"", b"a"), Some(0), "empty old");
        assert_eq!(first_diff(b"a", b""), Some(0), "empty new");
    }

    fn unique_temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("oxipresso-vfs-test-{nonce}"))
    }
}
