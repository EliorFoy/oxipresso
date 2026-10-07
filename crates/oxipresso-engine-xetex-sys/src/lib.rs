use std::os::raw::{c_char, c_int, c_void};

#[repr(C)]
pub struct OxiXetexConfig {
    pub root_dir: *const c_char,
    pub root_dir_len: usize,
    pub root_name: *const c_char,
    pub root_name_len: usize,
    pub format_path: *const c_char,
    pub format_path_len: usize,
    pub primary_name: *const c_char,
    pub primary_name_len: usize,
    pub build_date: u64,
    pub stream_mode: c_int,
    pub in_initex_mode: c_int,
    pub synctex_enabled: c_int,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OxiXetexResult {
    pub status: c_int,
    pub page_count: u32,
}

pub type OpenReadCallback = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        path: *const c_char,
        path_len: usize,
        kind: c_int,
        handle: *mut u32,
    ) -> c_int,
>;
pub type OpenWriteCallback = OpenReadCallback;
pub type ReadCallback = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        handle: u32,
        offset: usize,
        len: usize,
        bytes: *mut *const u8,
        out_len: *mut usize,
    ) -> c_int,
>;
pub type AppendCallback = Option<
    unsafe extern "C" fn(userdata: *mut c_void, handle: u32, bytes: *const u8, len: usize) -> c_int,
>;
pub type SizeCallback =
    Option<unsafe extern "C" fn(userdata: *mut c_void, handle: u32, size: *mut usize) -> c_int>;
pub type SeenCallback = Option<
    unsafe extern "C" fn(userdata: *mut c_void, handle: u32, offset: usize, engine_time: u64),
>;
pub type FlushCallback = Option<unsafe extern "C" fn(userdata: *mut c_void, handle: u32) -> c_int>;
pub type CloseCallback = Option<unsafe extern "C" fn(userdata: *mut c_void, handle: u32) -> c_int>;
pub type DiagnosticCallback = Option<
    unsafe extern "C" fn(userdata: *mut c_void, severity: c_int, bytes: *const u8, len: usize),
>;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OxiXetexCallbacks {
    pub userdata: *mut c_void,
    pub open_read: OpenReadCallback,
    pub open_write: OpenWriteCallback,
    pub read: ReadCallback,
    pub append: AppendCallback,
    pub size: SizeCallback,
    pub seen: SeenCallback,
    pub flush: FlushCallback,
    pub close: CloseCallback,
    pub diagnostic: DiagnosticCallback,
    pub fence: Option<unsafe extern "C" fn(userdata: *mut c_void) -> c_int>,
    /// P0 fast-resume policy probe: called at a firing fence before any
    /// capture. 1 = capture + park here, 0 = continue (absorbed or skip).
    pub fence_ex: Option<
        unsafe extern "C" fn(userdata: *mut c_void, path: *const c_char, cursor: u64) -> c_int,
    >,
}

unsafe extern "C" {
    pub fn oxipresso_xetex_run(
        config: *const OxiXetexConfig,
        callbacks: *const OxiXetexCallbacks,
        result: *mut OxiXetexResult,
    ) -> c_int;
    pub fn oxipresso_xetex_is_real() -> c_int;
    pub fn oxipresso_xetex_snapshot_bytes() -> u64;
    pub fn oxipresso_xetex_snapshot_capture(dst: *mut c_void, dst_len: u64) -> i64;
    pub fn oxipresso_xetex_request_fence_snapshot();
    pub fn oxipresso_xetex_fence_snapshot_len() -> u64;
    pub fn oxipresso_xetex_fence_snapshot_copy(dst: *mut c_void, dst_len: u64) -> u64;
    pub fn oxipresso_xetex_request_fence_roundtrip();
    pub fn oxipresso_xetex_fence_roundtrip_fired() -> u64;
    pub fn oxipresso_xetex_request_fence_restore();
    pub fn oxipresso_xetex_fence_restore_fired() -> u64;
    pub fn oxipresso_xetex_request_fence_park();
    /// Re-arm the sticky park-mode fence so the current run checkpoints and
    /// replays again at its next non-format read (multi-cycle chaining).
    pub fn oxipresso_xetex_arm_fence_replay();
    /// Enable resident passes: the patched host re-runs start_input +
    /// main_control from an S0 checkpoint while the controller commands it.
    pub fn oxipresso_xetex_enable_resident_passes();
    pub fn oxipresso_xetex_disable_resident_passes();
    /// 0 = mid-run fence park, 1 = resident pass-boundary park (read inside
    /// the fence callback to pick the right mirror-rollback target).
    pub fn oxipresso_xetex_fence_park_kind() -> u64;
    /// P0 fast-resume: name the file whose reads may park as the idle
    /// checkpoint, and the cursor the next edit is expected at/after.
    pub fn oxipresso_xetex_set_fence_target(path: *const c_char, cursor: u64);
    /// Flag that an edit is queued but not yet consumed: mid-pass reads of
    /// the edited file then fire the fence so the edit can be absorbed.
    pub fn oxipresso_xetex_set_edit_pending(pending: c_int);
    /// Hang-triage counters: resident pass-boundary parks and non-format
    /// reads, cumulative while resident mode is enabled.
    pub fn oxipresso_xetex_resident_debug_counts(parks: *mut u64, reads: *mut u64);
}
