use std::{fmt, path::PathBuf};

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    message: String,
}

impl EngineError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EngineError {}

impl From<std::io::Error> for EngineError {
    fn from(value: std::io::Error) -> Self {
        Self::new(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootDocument {
    pub root_dir: PathBuf,
    pub root_name: String,
    pub include_paths: Vec<PathBuf>,
    pub stream_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInit {
    pub engine_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    Idle,
    DocumentUpdated(DocumentArtifact),
    DiagnosticsUpdated,
    Terminated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartPolicy {
    NoRestartNeeded,
    RestartRequired,
    FullRestartRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactKind {
    Pdf,
    Xdv,
    Dvi,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentArtifact {
    pub kind: ArtifactKind,
    pub bytes: Vec<u8>,
    pub source_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTexArtifact {
    pub bytes: Vec<u8>,
    pub compressed: bool,
    pub source_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub path: Option<String>,
    pub line: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileHandle(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Afm,
    Bib,
    Bst,
    Cmap,
    Cnf,
    Enc,
    Format,
    FontMap,
    MiscFonts,
    Ofm,
    OpenType,
    Ovf,
    Picture,
    Pk,
    ProgramData,
    Sfd,
    Primary,
    Tex,
    TexPsHeader,
    Tfm,
    TrueType,
    Type1,
    Vf,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenResult {
    Opened {
        handle: FileHandle,
        canonical_path: String,
    },
    Missing,
    Promised,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PictureKey {
    pub path: String,
    pub picture_type: i32,
    pub page: i32,
}

pub trait EngineIo {
    fn open_read(&mut self, path: &str, kind: FileKind) -> Result<OpenResult>;
    fn open_write(&mut self, path: &str, kind: FileKind) -> Result<FileHandle>;
    fn read(&mut self, handle: FileHandle, offset: usize, len: usize) -> Result<Vec<u8>>;
    fn size(&mut self, handle: FileHandle) -> Result<usize>;
    fn append(&mut self, handle: FileHandle, bytes: &[u8]) -> Result<()>;
    fn seen(&mut self, handle: FileHandle, offset: usize, engine_time: u64);
    fn close(&mut self, handle: FileHandle) -> Result<()>;
    fn picture_bounds_get(&mut self, key: &PictureKey) -> Option<[f32; 4]>;
    fn picture_bounds_set(&mut self, key: PictureKey, bounds: [f32; 4]);
    fn snapshot_inputs(&mut self) -> Result<Vec<(String, Vec<u8>)>> {
        Ok(Vec::new())
    }
}

pub trait TypesettingEngine {
    fn initialize(&mut self, root: &RootDocument, io: &mut dyn EngineIo) -> Result<EngineInit>;
    fn step(&mut self, io: &mut dyn EngineIo) -> Result<EngineEvent>;
    fn apply_change_hint(
        &mut self,
        changed_file: &PathId,
        byte_offset: usize,
    ) -> Result<RestartPolicy>;
    fn restart(&mut self, io: &mut dyn EngineIo) -> Result<()>;
    fn output_document(&self) -> Option<DocumentArtifact>;
    fn output_synctex(&self) -> Option<SyncTexArtifact> {
        None
    }
    fn diagnostics(&self) -> &[Diagnostic];
}
