#include "tectonic_bridge_core.h"

#include <setjmp.h>
#include <stdio.h>

typedef struct {
  const char *root_dir;
  size_t root_dir_len;
  const char *root_name;
  size_t root_name_len;
  const char *format_path;
  size_t format_path_len;
  const char *primary_name;
  size_t primary_name_len;
  uint64_t build_date;
  int stream_mode;
  int in_initex_mode;
  int synctex_enabled;
} oxi_xetex_config;

typedef struct {
  int status;
  uint32_t page_count;
} oxi_xetex_result;

typedef struct {
  void *userdata;
  int (*open_read)(void *userdata, const char *path, size_t path_len, int kind, uint32_t *handle);
  int (*open_write)(void *userdata, const char *path, size_t path_len, int kind, uint32_t *handle);
  int (*read)(void *userdata, uint32_t handle, size_t offset, size_t len, const uint8_t **bytes, size_t *out_len);
  int (*append)(void *userdata, uint32_t handle, const uint8_t *bytes, size_t len);
  int (*size)(void *userdata, uint32_t handle, size_t *size);
  void (*seen)(void *userdata, uint32_t handle, size_t offset, uint64_t engine_time);
  int (*flush)(void *userdata, uint32_t handle);
  int (*close)(void *userdata, uint32_t handle);
  void (*diagnostic)(void *userdata, int severity, const uint8_t *bytes, size_t len);
  int (*fence)(void *userdata);
  /* Fast-resume policy probe (P0): called at a firing fence BEFORE any
   * capture. Returns 1 = capture + park here (the plain `fence` callback is
   * then invoked for the park phase), 0 = continue (an absorbable edit was
   * already injected by the callback, or the fence is below target). */
  int (*fence_ex)(void *userdata, const char *path, uint64_t cursor);
} oxi_xetex_callbacks;

typedef struct {
  const oxi_xetex_config *config;
  const oxi_xetex_callbacks *callbacks;
  char *root_name;
  char *format_path;
  char *primary_name;
  char *last_input_path;
  jmp_buf abort_jump;
  int abort_active;
} oxi_session;

struct ttbc_input_handle_t {
  uint32_t handle;
  size_t cursor;
  int ungot;
  char *path;
};

struct ttbc_output_handle_t {
  uint32_t handle;
};

struct ttbc_diagnostic_t {
  int severity;
  char *bytes;
  size_t len;
  size_t cap;
};

extern int tt_engine_xetex_main(const char *dump_name,
                                const char *input_file_name,
                                uint64_t build_date);
extern int tt_xetex_set_int_variable(const char *var_name, int value);

/* Mirror of the original engine main formats.c: TeX file names commonly omit
 * extensions, so failed input opens are retried with per-format extensions. */
static const char *exts_enc[] = {".enc", NULL};
static const char *exts_font_map[] = {".map", NULL};
static const char *exts_tfm[] = {".tfm", NULL};
static const char *exts_vf[] = {".vf", NULL};
static const char *exts_true_type[] = {".ttf", NULL};
static const char *exts_type1[] = {".pfb", NULL};
static const char *exts_open_type[] = {".otf", NULL};
static const char *exts_tex[] = {".tex", NULL};
static const char *exts_none[] = {NULL};

static const char **format_extensions(ttbc_file_format format) {
  switch (format) {
  case TTBC_FILE_FORMAT_ENC:
    return exts_enc;
  case TTBC_FILE_FORMAT_FONT_MAP:
    return exts_font_map;
  case TTBC_FILE_FORMAT_TFM:
    return exts_tfm;
  case TTBC_FILE_FORMAT_VF:
    return exts_vf;
  case TTBC_FILE_FORMAT_TRUE_TYPE:
    return exts_true_type;
  case TTBC_FILE_FORMAT_TYPE1:
    return exts_type1;
  case TTBC_FILE_FORMAT_OPEN_TYPE:
    return exts_open_type;
  case TTBC_FILE_FORMAT_TEX:
    return exts_tex;
  default:
    return exts_none;
  }
}

static oxi_session *active_session = NULL;

/* ---- At-fence read-only pool capture (checkpoint increment (a)) ----------
 * The engine frees ALL pool arrays at run exit (xetex-ini.c load_fmt cleanup),
 * so a snapshot is only valid *during* a run. TeXpresso forks at its input
 * read "fences"; our equivalent capture point is the first read of a
 * non-format file (post `.fmt` load, so `mem`/`str_pool`/`font_info` are
 * allocated and live). A test arms a one-shot capture; the read bridge runs
 * it into a static buffer. This exercises the capture path mid-run; paired
 * with the XDV identity oracle it proves the memcpy capture does not perturb
 * typesetting (the read-only property increment (a) must establish before
 * increment (b) adds restore+longjmp). */
static int g_fence_request = 0;
static int g_fence_park_mode = 0; /* sticky: park at every armed fence */
static int g_fence_park_kind = 0; /* 0=mid-run fence, 1=resident pass boundary */

/* ---- P0 fast-resume state ------------------------------------------------
 * Two checkpoint buffers coexist:
 *   g_fence_buf_s0 - the pre-start_input S0 capture, made ONCE by
 *     oxipresso_resident_capture(); every pass-boundary replay restores it
 *     (the proven full-replay path).
 *   g_fence_buf    - the MID-RUN capture taken when the fence parks at the
 *     target cursor (background replay idle state). The fast path restores
 *     it at the SAME live fence frame (identity) and longjmps back into the
 *     read, so only [target, EOF] is re-typeset - the in-process equivalent
 *     of TeXpresso resuming the forked child nearest the edit.
 * g_fence_tp_active arms the target park for ONE park; g_fence_edit_pending
 * makes every non-format read fire the fence so an edit arriving MID-PASS is
 * absorbed at the first read of its own file whose cursor is at/below the
 * edit offset (consumed prefix identical => output prefix identical). */
static uint8_t *g_fence_buf_s0 = NULL;
static uint64_t g_fence_len_s0 = 0;
static int32_t g_sa_root_saved_s0[8];
static int32_t g_cur_mark_saved_s0[5];
static volatile int g_fence_tp_active = 0;
static char *g_fence_target_path = NULL;
static uint64_t g_fence_target_cursor = 0;
static volatile int g_fence_edit_pending = 0;
static int g_resident_enabled = 0;
/* Observability for the pass loop (hang triage): parks completed and
 * non-format reads performed, cumulative while resident mode is on. */
static volatile uint64_t g_res_parks = 0;
static volatile uint64_t g_res_reads = 0;
static volatile uint64_t g_res_appends = 0;
static volatile unsigned char g_res_last_out = 0;
static uint8_t *g_fence_buf = NULL;
static uint64_t g_fence_len = 0;

static void oxi_fence_capture(void); /* defined with the snapshot helpers */

/* Increment (b1): a live-frame setjmp/longjmp round-trip at the fence. The
 * engine suspends exactly inside this read callback in TeXpresso's fork
 * model; proving we can setjmp here, longjmp back INTO THE SAME LIVE FRAME,
 * and the run still completes byte-identically is the prerequisite for (b2)
 * restore+longjmp. Note the frame-lifetime constraint this also pins: the
 * jmp_buf is only valid while the fence callback is parked, so (b2) will
 * additionally need the engine on a worker thread paused AT the fence
 * (an event loop), not merely captured-and-returned. */
static jmp_buf g_fence_jmp;
static int g_fence_roundtrip_request = 0;
static int g_fence_roundtrip_fired = 0;

void oxipresso_xetex_request_fence_roundtrip(void) {
  g_fence_roundtrip_request = 1;
  g_fence_roundtrip_fired = 0;
  g_fence_park_mode = 0;
  g_fence_request = 1;
}

uint64_t oxipresso_xetex_fence_roundtrip_fired(void) {
  return (uint64_t)g_fence_roundtrip_fired;
}

/* Increment (b2-mech): restore the captured pools over the live ones and
 * longjmp back to the fence, so the engine REPLAYS from the checkpoint.
 * Done inside the fence callback, which already blocks the engine — the
 * worker-thread park is only needed for an interactive controller, not to
 * validate the restore+replay mechanism itself. */
static int g_fence_restore_request = 0;
static int g_fence_restore_fired = 0;

static int oxi_fence_restore(int allow_grow); /* defined with the snapshot helpers */

void oxipresso_xetex_request_fence_restore(void) {
  g_fence_restore_request = 1;
  g_fence_restore_fired = 0;
  g_fence_park_mode = 0;
  g_fence_request = 1;
}

uint64_t oxipresso_xetex_fence_restore_fired(void) {
  return (uint64_t)g_fence_restore_fired;
}

/* Increment (b2-loop): instead of restoring inside the callback directly,
 * ask Rust's fence callback what to do. It runs ON the engine thread while
 * the read is parked, may block until the controller submits an edited
 * buffer (which it injects through its own &mut EngineIo borrow), and
 * returns 2 to restore+replay, 3 to continue unmodified (or a refused
 * restore also continues). */
void oxipresso_xetex_request_fence_park(void) {
  g_fence_park_mode = 1;
  g_fence_request = 1; /* the park path replays from a fresh capture */
  free(g_fence_buf);
  g_fence_buf = NULL;
  g_fence_len = 0;
}

/* Multi-cycle chaining: re-arm the (park-mode) fence so the CURRENT run
 * checkpoints+replays again at its next non-format read. One arm = one
 * park; call again to chain further edits into the same run. */
void oxipresso_xetex_arm_fence_replay(void) { g_fence_request = 1; }

/* P0 fast-resume controls. The target names the file whose reads may park
 * (the edited file) and the cursor the edit is expected at or after; the
 * controller sets both before commanding a background replay. */
static char *oxi_dup_cstr(const char *s) {
  if (s == NULL) {
    return NULL;
  }
  size_t n = strlen(s) + 1;
  char *copy = (char *)malloc(n);
  if (copy != NULL) {
    memcpy(copy, s, n);
  }
  return copy;
}

void oxipresso_xetex_set_fence_target(const char *path, uint64_t cursor) {
  free(g_fence_target_path);
  g_fence_target_path = oxi_dup_cstr(path);
  g_fence_target_cursor = cursor;
}

void oxipresso_xetex_set_edit_pending(int pending) {
  g_fence_edit_pending = pending;
}

void oxipresso_xetex_request_fence_snapshot(void) {
  g_fence_request = 1;
  g_fence_park_mode = 0;
  free(g_fence_buf);
  g_fence_buf = NULL;
  g_fence_len = 0;
}

uint64_t oxipresso_xetex_fence_snapshot_len(void) { return g_fence_len; }

/* Copy the fence-captured bytes into dst (up to dst_len). Returns bytes
 * copied, or 0 if no capture happened. */
uint64_t oxipresso_xetex_fence_snapshot_copy(void *dst, uint64_t dst_len) {
  if (!g_fence_buf || dst == NULL || dst_len == 0) {
    return 0;
  }
  uint64_t n = g_fence_len < dst_len ? g_fence_len : dst_len;
  memcpy(dst, g_fence_buf, n);
  return n;
}
static const char *last_error_message = "";

static char *copy_slice(const char *bytes, size_t len) {
  char *copy = (char *)calloc(len + 1, 1);
  if (!copy) {
    return NULL;
  }
  if (bytes && len > 0) {
    memcpy(copy, bytes, len);
  }
  copy[len] = 0;
  return copy;
}

static void emit_diagnostic(int severity, const char *bytes, size_t len) {
  if (active_session && active_session->callbacks && active_session->callbacks->diagnostic) {
    active_session->callbacks->diagnostic(active_session->callbacks->userdata,
                                          severity,
                                          (const uint8_t *)bytes,
                                          len);
  }
}

static void emit_cstr_diagnostic(int severity, const char *text) {
  if (!text) {
    text = "";
  }
  emit_diagnostic(severity, text, strlen(text));
}

static void diag_append_bytes(ttbc_diagnostic_t *diag, const char *bytes, size_t len) {
  if (!diag || !bytes || len == 0) {
    return;
  }
  size_t needed = diag->len + len + 1;
  if (needed > diag->cap) {
    size_t next_cap = diag->cap ? diag->cap * 2 : 128;
    while (next_cap < needed) {
      next_cap *= 2;
    }
    char *next = (char *)realloc(diag->bytes, next_cap);
    if (!next) {
      return;
    }
    diag->bytes = next;
    diag->cap = next_cap;
  }
  memcpy(diag->bytes + diag->len, bytes, len);
  diag->len += len;
  diag->bytes[diag->len] = 0;
}

static ttbc_diagnostic_t *diag_new(int severity) {
  ttbc_diagnostic_t *diag = (ttbc_diagnostic_t *)calloc(1, sizeof(ttbc_diagnostic_t));
  if (diag) {
    diag->severity = severity;
  }
  return diag;
}

const char *_ttbc_get_error_message(void) {
  return last_error_message;
}

void ttbc_issue_warning(const char *text) {
  emit_cstr_diagnostic(1, text);
}

void ttbc_issue_error(const char *text) {
  emit_cstr_diagnostic(2, text);
}

ttbc_diagnostic_t *ttbc_diag_begin_warning(void) {
  return diag_new(1);
}

ttbc_diagnostic_t *ttbc_diag_begin_error(void) {
  return diag_new(2);
}

void ttbc_diag_append(ttbc_diagnostic_t *diag, const char *text) {
  if (text) {
    diag_append_bytes(diag, text, strlen(text));
  }
}

void ttstub_diag_finish(ttbc_diagnostic_t *diag) {
  if (!diag) {
    return;
  }
  emit_diagnostic(diag->severity, diag->bytes ? diag->bytes : "", diag->len);
  free(diag->bytes);
  free(diag);
}

void ttbc_diag_finish(ttbc_diagnostic_t *diag) {
  ttstub_diag_finish(diag);
}

void ttstub_diag_vprintf(ttbc_diagnostic_t *diag, const char *format, va_list ap) {
  va_list copy;
  va_copy(copy, ap);
  int len = vsnprintf(NULL, 0, format, copy);
  va_end(copy);
  if (len <= 0) {
    return;
  }
  char *buffer = (char *)malloc((size_t)len + 1);
  if (!buffer) {
    return;
  }
  vsnprintf(buffer, (size_t)len + 1, format, ap);
  diag_append_bytes(diag, buffer, (size_t)len);
  free(buffer);
}

void ttstub_diag_printf(ttbc_diagnostic_t *diag, const char *format, ...) {
  va_list ap;
  va_start(ap, format);
  ttstub_diag_vprintf(diag, format, ap);
  va_end(ap);
}

void ttstub_issue_warning(const char *format, ...) {
  ttbc_diagnostic_t *diag = ttbc_diag_begin_warning();
  va_list ap;
  va_start(ap, format);
  ttstub_diag_vprintf(diag, format, ap);
  va_end(ap);
  ttstub_diag_finish(diag);
}

void ttstub_issue_error(const char *format, ...) {
  ttbc_diagnostic_t *diag = ttbc_diag_begin_error();
  va_list ap;
  va_start(ap, format);
  ttstub_diag_vprintf(diag, format, ap);
  va_end(ap);
  ttstub_diag_finish(diag);
}

int _tt_abort(const char *format, ...) {
  static char message[2048];
  va_list ap;
  va_start(ap, format);
  vsnprintf(message, sizeof(message), format, ap);
  va_end(ap);
  message[sizeof(message) - 1] = 0;
  last_error_message = message;
  emit_cstr_diagnostic(2, message);
  if (active_session && active_session->abort_active) {
    longjmp(active_session->abort_jump, 1);
  }
  abort();
}

static rust_output_handle_t output_open_kind(char const *path, int is_gz, ttbc_file_format format) {
  (void)is_gz;
  if (!active_session || !active_session->callbacks || !active_session->callbacks->open_write || !path) {
    return NULL;
  }
  uint32_t handle = 0;
  if (active_session->callbacks->open_write(active_session->callbacks->userdata,
                                            path,
                                            strlen(path),
                                            (int)format,
                                            &handle) != 0) {
    return NULL;
  }
  ttbc_output_handle_t *output = (ttbc_output_handle_t *)calloc(1, sizeof(ttbc_output_handle_t));
  if (!output) {
    active_session->callbacks->close(active_session->callbacks->userdata, handle);
    return NULL;
  }
  output->handle = handle;
  return output;
}

rust_output_handle_t ttstub_output_open(char const *path, int is_gz) {
  return output_open_kind(path, is_gz, TTBC_FILE_FORMAT_PROGRAM_DATA);
}

rust_output_handle_t ttstub_output_open_format(char const *path, int is_gz) {
  return output_open_kind(path, is_gz, TTBC_FILE_FORMAT_FORMAT);
}

rust_output_handle_t ttstub_output_open_stdout(void) {
  return ttstub_output_open("stdout", 0);
}

int ttstub_output_putc(rust_output_handle_t handle, int c) {
  unsigned char byte = (unsigned char)c;
  return ttstub_output_write(handle, (const char *)&byte, 1) == 1 ? byte : EOF;
}

size_t ttstub_output_write(rust_output_handle_t handle, const char *data, size_t len) {
  if (!active_session || !active_session->callbacks || !active_session->callbacks->append ||
      !handle || (!data && len > 0)) {
    return 0;
  }
  if (g_resident_enabled) {
    g_res_appends += (uint64_t)len;
    if (len > 0 && data) {
      g_res_last_out = data[len - 1];
    }
    if ((g_res_appends & 0xFFFFFu) < (uint64_t)len) {
      fprintf(stderr, "[oxi] appends=%llu last=%02X\n",
              (unsigned long long)g_res_appends,
              (unsigned)g_res_last_out);
      fflush(stderr);
    }
  }
  if (active_session->callbacks->append(active_session->callbacks->userdata,
                                        handle->handle,
                                        (const uint8_t *)data,
                                        len) != 0) {
    return 0;
  }
  return len;
}

int ttstub_fprintf(rust_output_handle_t handle, const char *format, ...) {
  va_list ap;
  va_start(ap, format);
  va_list copy;
  va_copy(copy, ap);
  int len = vsnprintf(NULL, 0, format, copy);
  va_end(copy);
  if (len <= 0) {
    va_end(ap);
    return len;
  }
  char *buffer = (char *)malloc((size_t)len + 1);
  if (!buffer) {
    va_end(ap);
    return -1;
  }
  vsnprintf(buffer, (size_t)len + 1, format, ap);
  va_end(ap);
  size_t written = ttstub_output_write(handle, buffer, (size_t)len);
  free(buffer);
  return written == (size_t)len ? len : -1;
}

int ttstub_output_flush(rust_output_handle_t handle) {
  if (!active_session || !active_session->callbacks || !handle) {
    return 1;
  }
  if (active_session->callbacks->flush) {
    return active_session->callbacks->flush(active_session->callbacks->userdata, handle->handle);
  }
  return 0;
}

int ttstub_output_close(rust_output_handle_t handle) {
  if (!active_session || !active_session->callbacks || !active_session->callbacks->close || !handle) {
    return 1;
  }
  int result = active_session->callbacks->close(active_session->callbacks->userdata, handle->handle);
  free(handle);
  return result;
}

ttbc_output_handle_t *ttbc_output_open(const char *name, int is_gz) {
  return ttstub_output_open(name, is_gz);
}

ttbc_output_handle_t *ttbc_output_open_stdout(void) {
  return ttstub_output_open_stdout();
}

int ttbc_output_putc(ttbc_output_handle_t *handle, int c) {
  return ttstub_output_putc(handle, c);
}

size_t ttbc_output_write(ttbc_output_handle_t *handle, const uint8_t *data, size_t len) {
  return ttstub_output_write(handle, (const char *)data, len);
}

int ttbc_output_flush(ttbc_output_handle_t *handle) {
  return ttstub_output_flush(handle);
}

int ttbc_output_close(ttbc_output_handle_t *handle) {
  return ttstub_output_close(handle);
}

static rust_input_handle_t input_open_path(const char *path, ttbc_file_format format) {
  if (!active_session || !active_session->callbacks || !active_session->callbacks->open_read || !path) {
    return NULL;
  }

  const char *requested_path = path;
  if (format == TTBC_FILE_FORMAT_FORMAT && active_session->format_path && active_session->format_path[0]) {
    requested_path = active_session->format_path;
  }

  uint32_t handle = 0;
  const char *opened_path = requested_path;
  char *extension_candidate = NULL;
  int open_result = active_session->callbacks->open_read(active_session->callbacks->userdata,
                                                         requested_path,
                                                         strlen(requested_path),
                                                         (int)format,
                                                         &handle);

  /* Extension guessing, mirroring the original engine main: retry a failed
   * missing-file open once per format extension before giving up. Promised
   * files (result == 2) stay promised and are not retried. */
  if (open_result == 1) {
    for (const char **exts = format_extensions(format); *exts; exts++) {
      size_t base_len = strlen(requested_path);
      const char *extension = *exts;
      size_t extension_len = strlen(extension);
      if (base_len >= extension_len &&
          memcmp(requested_path + base_len - extension_len, extension, extension_len) == 0) {
        continue;
      }
      char *candidate = (char *)malloc(base_len + extension_len + 1);
      if (!candidate) {
        break;
      }
      memcpy(candidate, requested_path, base_len);
      memcpy(candidate + base_len, extension, extension_len + 1);
      open_result = active_session->callbacks->open_read(active_session->callbacks->userdata,
                                                         candidate,
                                                         base_len + extension_len,
                                                         (int)format,
                                                         &handle);
      if (open_result == 0) {
        free(extension_candidate);
        extension_candidate = candidate;
        opened_path = candidate;
        break;
      }
      free(candidate);
    }
  }

  if (open_result != 0) {
    free(extension_candidate);
    return NULL;
  }

  ttbc_input_handle_t *input = (ttbc_input_handle_t *)calloc(1, sizeof(ttbc_input_handle_t));
  if (!input) {
    active_session->callbacks->close(active_session->callbacks->userdata, handle);
    free(extension_candidate);
    return NULL;
  }
  input->handle = handle;
  input->ungot = -1;
  input->path = copy_slice(opened_path, strlen(opened_path));

  free(active_session->last_input_path);
  active_session->last_input_path = copy_slice(opened_path, strlen(opened_path));
  free(extension_candidate);
  return input;
}

rust_input_handle_t ttstub_input_open(char const *path, ttbc_file_format format, int is_gz) {
  (void)is_gz;
  return input_open_path(path, format);
}

rust_input_handle_t ttstub_input_open_primary(void) {
  if (!active_session) {
    return NULL;
  }
  /* Format bootstrap mode opens the format source (e.g. `xelatex.ini`) as a
   * regular TeX input, matching the original engine main bootstrap. */
  if (active_session->primary_name && active_session->primary_name[0]) {
    return input_open_path(active_session->primary_name, TTBC_FILE_FORMAT_TEX);
  }
  if (!active_session->root_name) {
    return NULL;
  }
  return input_open_path(active_session->root_name, TTBC_FILE_FORMAT_TECTONIC_PRIMARY);
}

ssize_t ttstub_get_last_input_abspath(char *buffer, size_t len) {
  if (!active_session || !active_session->last_input_path) {
    return 0;
  }
  size_t path_len = strlen(active_session->last_input_path) + 1;
  if (path_len > len) {
    return -2;
  }
  memcpy(buffer, active_session->last_input_path, path_len);
  return (ssize_t)path_len;
}

size_t ttstub_input_get_size(rust_input_handle_t handle) {
  if (!active_session || !active_session->callbacks || !active_session->callbacks->size || !handle) {
    return 0;
  }
  size_t size = 0;
  if (active_session->callbacks->size(active_session->callbacks->userdata, handle->handle, &size) != 0) {
    return 0;
  }
  return size;
}

time_t ttstub_input_get_mtime(rust_input_handle_t handle) {
  (void)handle;
  return 0;
}

size_t ttstub_input_seek(rust_input_handle_t handle, ssize_t offset, int whence) {
  if (!handle) {
    return 0;
  }
  size_t base = 0;
  if (whence == SEEK_CUR) {
    base = handle->cursor;
  } else if (whence == SEEK_END) {
    base = ttstub_input_get_size(handle);
  }

  if (offset < 0 && (size_t)(-offset) > base) {
    handle->cursor = 0;
  } else {
    handle->cursor = (size_t)((ssize_t)base + offset);
  }
  handle->ungot = -1;
  return handle->cursor;
}

ssize_t ttstub_input_read(rust_input_handle_t handle, char *data, size_t len) {
  if (!active_session || !active_session->callbacks || !active_session->callbacks->read ||
      !handle || (!data && len > 0)) {
    return -1;
  }
  size_t copied = 0;
  if (g_resident_enabled && handle->path &&
      !(active_session->format_path &&
        strcmp(handle->path, active_session->format_path) == 0)) {
    g_res_reads++;
  }
  /* Re-armable checkpoint fence (multi-cycle): every non-format read may park
   * while g_fence_request is armed (the .fmt has loaded, pools are live). One
   * arm consumes into exactly one capture+park+replay; the controller re-arms
   * (arm_fence_replay) to chain further checkpoints LATER in the same run —
   * the in-process equivalent of TeXpresso's per-fence fork tree. With the
   * round-trip armed (b1): setjmp, capture, longjmp back into this same live
   * frame, fall through to the read. The cursor has not advanced when
   * longjmp fires, so no bytes are skipped or doubled. */
  /* P0 fast-resume fence: fires when a target park is armed (the idle-state
   * checkpoint at the expected edit position) or when an edit is pending
   * mid-pass (absorption). The Rust policy callback decides: absorb an
   * editable read (inject + continue, NO capture), skip (the pending edit
   * belongs to another file), or park (capture + block at this exact read;
   * the edit then fast-resumes via restore+longjmp or defers to the next
   * pass boundary). */
  if (g_resident_enabled && handle->path &&
      !(active_session->format_path &&
        strcmp(handle->path, active_session->format_path) == 0) &&
      (g_fence_tp_active || g_fence_edit_pending)) {
    const int is_target =
        g_fence_target_path != NULL &&
        strcmp(handle->path, g_fence_target_path) == 0;
    int fire = g_fence_edit_pending ||
               (g_fence_tp_active && is_target &&
                handle->cursor >= g_fence_target_cursor);
    if (fire) {
      int was_pending = g_fence_edit_pending;
      int policy = 1;
      if (active_session->callbacks->fence_ex) {
        policy = active_session->callbacks->fence_ex(
            active_session->callbacks->userdata, handle->path,
            (uint64_t)handle->cursor);
      }
      if (policy == 0 && was_pending && !g_fence_edit_pending) {
        /* The edit was ABSORBED into this live pass (the callback injected
         * the new buffer and cleared the pending flag). The pass is no
         * longer a speculative background replay: it carries the edit and
         * must run to completion to deliver its artifact — leaving the
         * target arm active would park it at the target cursor and
         * deadlock the waiting controller. */
        g_fence_tp_active = 0;
      }
      if (policy == 1) {
        g_fence_tp_active = 0; /* one park per arm */
        int jr = setjmp(g_fence_jmp);
        if (jr == 0) {
          oxi_fence_capture();
          g_fence_park_kind = 0;
          int cmd = 0;
          if (active_session->callbacks->fence) {
            cmd = active_session->callbacks->fence(
                active_session->callbacks->userdata);
          }
          if (cmd == 2) {
            if (oxi_fence_restore(0) == 0) {
              longjmp(g_fence_jmp, 2); /* fast resume at this very fence */
            }
          }
          /* cmd 3 (continue old content / finish) or a refused restore:
           * fall through and keep reading the current buffer. */
        } else {
          g_fence_restore_fired++;
        }
      }
    }
  }
  if (g_fence_request && handle->path &&
      !(active_session->format_path &&
        strcmp(handle->path, active_session->format_path) == 0)) {
    int jr = setjmp(g_fence_jmp);
    if (jr == 0) {
      /* Consume the arm BEFORE the (blocking) controller callback, so a
       * concurrent re-arm can never be overwritten here. */
      g_fence_request = 0;
      g_fence_park_kind = 0; /* mid-run fence semantics */
      oxi_fence_capture();
      int cmd = 0;
      if (g_fence_park_mode && active_session->callbacks->fence) {
        cmd = active_session->callbacks->fence(
            active_session->callbacks->userdata);
      } else if (g_fence_restore_request) {
        g_fence_restore_request = 0;
        cmd = 2;
      } else if (g_fence_roundtrip_request) {
        g_fence_roundtrip_request = 0;
        cmd = 1;
      }
      if (cmd == 2) {
        if (oxi_fence_restore(0) == 0) {
          longjmp(g_fence_jmp, 2); /* replay the fence from the snapshot */
        }
      } else if (cmd == 1) {
        longjmp(g_fence_jmp, 1);
      }
      /* cmd == 3 (park "continue") or a refused restore: fall through. */
    } else if (jr == 2) {
      g_fence_restore_fired++; /* re-entered after a state restore */
    } else {
      g_fence_roundtrip_fired++; /* re-entered via a live-frame round-trip */
    }
  }
  if (handle->ungot >= 0 && len > 0) {
    data[0] = (char)handle->ungot;
    handle->ungot = -1;
    handle->cursor++;
    copied = 1;
  }
  if (copied == len) {
    return (ssize_t)copied;
  }

  const uint8_t *bytes = NULL;
  size_t out_len = 0;
  if (active_session->callbacks->read(active_session->callbacks->userdata,
                                      handle->handle,
                                      handle->cursor,
                                      len - copied,
                                      &bytes,
                                      &out_len) != 0) {
    return copied ? (ssize_t)copied : -1;
  }
  if (out_len > len - copied) {
    out_len = len - copied;
  }
  if (out_len > 0 && bytes) {
    memcpy(data + copied, bytes, out_len);
  }
  handle->cursor += out_len;
  return (ssize_t)(copied + out_len);
}

int ttstub_input_getc(rust_input_handle_t handle) {
  unsigned char byte = 0;
  ssize_t read = ttstub_input_read(handle, (char *)&byte, 1);
  if (read == 1) {
    return byte;
  }
  return EOF;
}

int ttstub_input_ungetc(rust_input_handle_t handle, int ch) {
  if (!handle || ch == EOF || handle->ungot >= 0) {
    return EOF;
  }
  handle->ungot = ch & 0xff;
  if (handle->cursor > 0) {
    handle->cursor--;
  }
  return ch;
}

int ttstub_input_close(rust_input_handle_t handle) {
  if (!active_session || !active_session->callbacks || !active_session->callbacks->close || !handle) {
    return 1;
  }
  if (active_session->callbacks->seen) {
    size_t size = ttstub_input_get_size(handle);
    size_t seen = size > handle->cursor ? size : handle->cursor;
    active_session->callbacks->seen(active_session->callbacks->userdata, handle->handle, seen, 0);
  }
  int result = active_session->callbacks->close(active_session->callbacks->userdata, handle->handle);
  free(handle->path);
  free(handle);
  return result;
}

ttbc_input_handle_t *ttbc_input_open(const char *name, ttbc_file_format format, int is_gz) {
  return ttstub_input_open(name, format, is_gz);
}

ttbc_input_handle_t *ttbc_input_open_primary(void) {
  return ttstub_input_open_primary();
}

ssize_t ttbc_get_last_input_abspath(uint8_t *buffer, size_t len) {
  return ttstub_get_last_input_abspath((char *)buffer, len);
}

size_t ttbc_input_get_size(ttbc_input_handle_t *handle) {
  return ttstub_input_get_size(handle);
}

int64_t ttbc_input_get_mtime(ttbc_input_handle_t *handle) {
  return (int64_t)ttstub_input_get_mtime(handle);
}

size_t ttbc_input_seek(ttbc_input_handle_t *handle, ssize_t offset, int whence, int *internal_error) {
  if (internal_error) {
    *internal_error = 0;
  }
  return ttstub_input_seek(handle, offset, whence);
}

int ttbc_input_getc(ttbc_input_handle_t *handle) {
  return ttstub_input_getc(handle);
}

int ttbc_input_ungetc(ttbc_input_handle_t *handle, int ch) {
  return ttstub_input_ungetc(handle, ch);
}

ssize_t ttbc_input_read(ttbc_input_handle_t *handle, uint8_t *data, size_t len) {
  return ttstub_input_read(handle, (char *)data, len);
}

int ttbc_input_close(ttbc_input_handle_t *handle) {
  return ttstub_input_close(handle);
}

int ttstub_pic_get_cached_bounds(const char *name, int type, int page, float bounds[4]) {
  (void)name;
  (void)type;
  (void)page;
  (void)bounds;
  return 0;
}

void ttstub_pic_set_cached_bounds(const char *name, int type, int page, const float bounds[4]) {
  (void)name;
  (void)type;
  (void)page;
  (void)bounds;
}

int ttbc_get_file_md5(const char *path, uint8_t *digest) {
  return ttstub_get_file_md5(path, (char *)digest);
}

int ttstub_shell_escape(const unsigned short *cmd, size_t len) {
  (void)cmd;
  (void)len;
  emit_cstr_diagnostic(1, "shell escape disabled by oxipresso real XeTeX FFI shim");
  return 1;
}

int ttbc_shell_escape(const uint16_t *cmd, size_t len) {
  return ttstub_shell_escape((const unsigned short *)cmd, len);
}

int oxipresso_xetex_is_real(void) {
  return 1;
}

/* ---- Read-only engine pool snapshot (checkpoint increment (a)) ----------
 * Header: 12 little-endian u64 words -- five pool byte-sizes, five reserved/
 * zero slots kept for header stability, then five cursors interleaved in the
 * original slots -- followed by the pool data blocks in fixed order:
 *   mem, str_start, str_pool, save_stack, font_info.
 * NOT included: `eqtb`. TeXpresso's eqtb array is indexed to the VIRTUAL
 * `eqtb_top` (~9.5M here) while only ~`eqtb_size` (300K) base entries are
 * allocated and every eqtb[i] is a pointer to its own equivalent segment on
 * the heap -- a pointer graph, not a POD arena. (An earlier draft memcpy'd
 * `eqtb_top+1` entries and walked straight off the allocation: the access
 * violation this comment documents.) eqtb state belongs to the later
 * global-state layer of the checkpoint design, never to this pool capture.
 * Pure reads of the engine's globals. Valid ONLY during a run: the engine
 * frees every pool array at run exit, so the capture must happen at a fence
 * (see oxi_fence_capture / the read bridge), never between runs. NULL pools /
 * non-positive cursors yield zero-sized blocks. */
#include "xetex-xetexd.h"
#include <string.h>
#include <stdlib.h>

static uint64_t oxi_snap_sizes(uint64_t sizes[5], const void *bases[5]) {
  const long long entries[5] = {
    (long long)mem_end + 1,
    (long long)str_ptr + 2,
    (long long)pool_ptr + 2,
    (long long)save_ptr + 2,
    (long long)fmem_ptr + 2,
  };
  const size_t elem[5] = {
    sizeof(memory_word), sizeof(pool_pointer), sizeof(packed_UTF16_code),
    sizeof(memory_word), sizeof(memory_word),
  };
  bases[0] = mem;
  bases[1] = str_start;
  bases[2] = str_pool;
  bases[3] = save_stack;
  bases[4] = font_info;
  uint64_t total = 12 * (uint64_t)sizeof(uint64_t);
  for (int i = 0; i < 5; i++) {
    uint64_t s = (entries[i] <= 0 || !bases[i])
                     ? 0
                     : (uint64_t)entries[i] * (uint64_t)elem[i];
    sizes[i] = s;
    total += s;
  }
  return total;
}

/* Fill `dst` (>= need bytes) with the header + pool blocks. Caller has
 * already computed sizes/bases via oxi_snap_sizes. */
static void oxi_fill_snapshot(uint8_t *dst, uint64_t sizes[5],
                              const void *bases[5]) {
  uint64_t *hdr = (uint64_t *)dst;
  for (int i = 0; i < 5; i++) {
    hdr[i] = sizes[i];
  }
  /* Slots 5 and 7 are a fixed 0 (the former eqtb size/cursor pair) so the
   * 12-word header layout never shifts. */
  hdr[5] = 0;
  hdr[6] = (uint64_t)(int64_t)mem_end;
  hdr[7] = 0;
  hdr[8] = (uint64_t)(int64_t)str_ptr;
  hdr[9] = (uint64_t)(int64_t)pool_ptr;
  hdr[10] = (uint64_t)(int64_t)save_ptr;
  hdr[11] = (uint64_t)(int64_t)fmem_ptr;
  uint8_t *p = dst + 12 * (uint64_t)sizeof(uint64_t);
  for (int i = 0; i < 5; i++) {
    if (sizes[i] != 0) {
      memcpy(p, bases[i], sizes[i]);
      p += sizes[i];
    }
  }
}

uint64_t oxipresso_xetex_snapshot_bytes(void) {
  if (!active_session) {
    return 0; /* pools are freed outside a run */
  }
  uint64_t sizes[5];
  const void *bases[5];
  return oxi_snap_sizes(sizes, bases);
}

int64_t oxipresso_xetex_snapshot_capture(void *dst, uint64_t dst_len) {
  if (!active_session || dst == NULL) {
    return -1;
  }
  uint64_t sizes[5];
  const void *bases[5];
  uint64_t need = oxi_snap_sizes(sizes, bases);
  if (dst_len < need) {
    return -1;
  }
  oxi_fill_snapshot((uint8_t *)dst, sizes, bases);
  return (int64_t)need;
}

/* One-shot capture of the live pools into a static buffer, run from the
 * read bridge at the fence (active_session is non-NULL here). */
/* Document-level mark state lives OUTSIDE the five pools: sa_root is the
 * sparse-array root mutated by pass 1's \mark insertions, and cur_mark is
 * the running-head mark pair. Neither is ever touched by the fmt-load
 * undump stream, so the passive scalar recording never sees them - and an
 * unrestored sa_root walks pass-1 tree nodes into S0-overwritten memory
 * (do_marks -> delete_token_ref SEGV). Saved/restored explicitly. */
static int32_t g_sa_root_saved[8];
static int32_t g_cur_mark_saved[5];

static void oxi_fence_capture(void) {
  /* Fresh capture per arm: the previous checkpoint buffer is superseded. */
  free(g_fence_buf);
  g_fence_buf = NULL;
  g_fence_len = 0;
  memcpy(g_sa_root_saved, sa_root, sizeof(g_sa_root_saved));
  memcpy(g_cur_mark_saved, cur_mark, sizeof(g_cur_mark_saved));
  uint64_t sizes[5];
  const void *bases[5];
  uint64_t need = oxi_snap_sizes(sizes, bases);
  uint8_t *buf = (uint8_t *)malloc((size_t)need);
  if (buf == NULL) {
    return;
  }
  oxi_fill_snapshot(buf, sizes, bases);
  g_fence_buf = buf;
  g_fence_len = need;
}

/* S0 variant: captures into the dedicated pre-document buffer so a later
 * mid-run capture cannot supersede the pass-boundary replay checkpoint.
 * The mark state is kept per-buffer (a mid-run capture must not overwrite
 * the S0 marks the boundary restore needs). */
static void oxi_fence_capture_s0(void) {
  free(g_fence_buf_s0);
  g_fence_buf_s0 = NULL;
  g_fence_len_s0 = 0;
  memcpy(g_sa_root_saved_s0, sa_root, sizeof(g_sa_root_saved_s0));
  memcpy(g_cur_mark_saved_s0, cur_mark, sizeof(g_cur_mark_saved_s0));
  uint64_t sizes[5];
  const void *bases[5];
  uint64_t need = oxi_snap_sizes(sizes, bases);
  uint8_t *buf = (uint8_t *)malloc((size_t)need);
  if (buf == NULL) {
    return;
  }
  oxi_fill_snapshot(buf, sizes, bases);
  g_fence_buf_s0 = buf;
  g_fence_len_s0 = need;
}

/* Copy the captured bytes back over the pools they were captured from.
 * The engine is frozen inside the fence callback, so the live pool bases
 * must be non-NULL and no pool may have SHRUNK below its captured size;
 * any such mismatch refuses the write (a stale/foreign buffer must never
 * touch a live engine). With allow_grow (resident pass boundaries only),
 * pools that GREW during a completed pass are accepted: the web2c pools
 * are bump-allocated, the realloc keeps old contents, and restoring the
 * captured prefix plus the header cursors reproduces the exact captured
 * state - unreachable later bytes simply stay dead. */
static int oxi_fence_restore(int allow_grow) {
  if (!g_fence_buf) {
    return -1;
  }
  const uint8_t *buf = g_fence_buf;
  const uint64_t len = g_fence_len;
  uint64_t sizes[5];
  const void *bases[5];
  uint64_t need = oxi_snap_sizes(sizes, bases);
  const uint64_t *hdr = (const uint64_t *)buf;
  uint64_t total = 12 * (uint64_t)sizeof(uint64_t);
  for (int i = 0; i < 5; i++) {
    if (allow_grow) {
      if (hdr[i] > sizes[i]) {
        return -1; /* pools never legitimately shrink */
      }
    } else if (hdr[i] != sizes[i]) {
      return -1;
    }
    total += hdr[i];
  }
  if (len != total) {
    return -1; /* buffer must be exactly header + captured blocks */
  }
  memcpy(sa_root, g_sa_root_saved, sizeof(g_sa_root_saved));
  memcpy(cur_mark, g_cur_mark_saved, sizeof(g_cur_mark_saved));
  /* Restore the bump cursors (hdr slots 6 and 8..11; slots 5/7 are the
   * retired eqtb pair). Inside a frozen mid-run window these are already
   * the captured values; across a completed pass they are far ahead and
   * MUST rewind, or the restored prefixes would be shadowed by live junk. */
  mem_end = (int32_t)hdr[6];
  str_ptr = (str_number)hdr[8];
  pool_ptr = (pool_pointer)hdr[9];
  save_ptr = (int32_t)hdr[10];
  fmem_ptr = (font_index)hdr[11];
  const uint8_t *p = buf + 12 * (uint64_t)sizeof(uint64_t);
  for (int i = 0; i < 5; i++) {
    if (hdr[i] != 0) {
      memcpy((void *)bases[i], p, hdr[i]);
      p += hdr[i];
    }
  }
  return 0;
}

/* Pass-boundary variant: restores the S0 capture (pre-start_input state)
 * plus ITS OWN mark snapshot, so a later mid-run capture can never poison
 * the boundary replay. */
static int oxi_fence_restore_s0(int allow_grow) {
  if (!g_fence_buf_s0) {
    return -1;
  }
  const uint8_t *buf = g_fence_buf_s0;
  const uint64_t len = g_fence_len_s0;
  uint64_t sizes[5];
  const void *bases[5];
  uint64_t need = oxi_snap_sizes(sizes, bases);
  const uint64_t *hdr = (const uint64_t *)buf;
  uint64_t total = 12 * (uint64_t)sizeof(uint64_t);
  for (int i = 0; i < 5; i++) {
    if (allow_grow) {
      if (hdr[i] > sizes[i]) {
        return -1;
      }
    } else if (hdr[i] != sizes[i]) {
      return -1;
    }
    total += hdr[i];
  }
  if (len != total) {
    return -1;
  }
  memcpy(sa_root, g_sa_root_saved_s0, sizeof(g_sa_root_saved_s0));
  memcpy(cur_mark, g_cur_mark_saved_s0, sizeof(g_cur_mark_saved_s0));
  mem_end = (int32_t)hdr[6];
  str_ptr = (str_number)hdr[8];
  pool_ptr = (pool_pointer)hdr[9];
  save_ptr = (int32_t)hdr[10];
  fmem_ptr = (font_index)hdr[11];
  const uint8_t *p = buf + 12 * (uint64_t)sizeof(uint64_t);
  for (int i = 0; i < 5; i++) {
    if (hdr[i] != 0) {
      memcpy((void *)bases[i], p, hdr[i]);
      p += hdr[i];
    }
  }
  return 0;
}

/* ---- Resident passes (DESIGN-resume.md option b) ------------------------
 * The patched host (xetex-ini.c) calls these around its typesetting pass.
 * S0 = the checkpoint captured right BEFORE the first start_input: a
 * post-format-load, pre-document state. Each commanded replay restores S0
 * and re-runs start_input + main_control - the in-process analogue of
 * TeXpresso forking a fresh child from the parked parent per rebuild.
 * The 0/1 park-kind channel below tells the Rust fence callback which
 * checkpoint semantics apply (mid-run fence vs pass boundary), so the
 * mirror rollback targets the right capture. */
void oxipresso_xetex_resident_debug_counts(uint64_t *parks, uint64_t *reads) {
  if (parks) {
    *parks = g_res_parks;
  }
  if (reads) {
    *reads = g_res_reads;
  }
}

/* ---- Scalar-global rewind set (the state web2c itself calls "the format")
 * The patched host reports every do_undump target while the .fmt loads;
 * that address/size stream IS the complete rewindable surface (eqtb_top,
 * hash[], the fixed arrays, every scalar - by the format's own definition,
 * no hand list to maintain). Recording runs from fmt-load start until the
 * S0 capture; the regions stay allocated until the deferred cleanup, so
 * replaying the recorded bytes at each pass boundary rewinds exactly what
 * the five pools + cursors do not cover. */
#define OXI_SCALAR_MAX 65536
typedef struct {
  const unsigned char *p;
  uint64_t n;
} oxi_scalar_reg;
static oxi_scalar_reg g_scalar_regs[OXI_SCALAR_MAX];
static int g_scalar_count = 0;
static int g_scalar_overflow = 0;
static int g_scalars_recording = 0;
static uintptr_t g_stack_lo = 0, g_stack_hi = 0; /* engine-thread window */
static uint8_t *g_scalar_buf = NULL;
static uint64_t g_scalar_data = 0; /* payload bytes behind an 2-word header */

void oxipresso_undump_record(const void *p, size_t n) {
  if (!g_scalars_recording || n == 0) {
    return;
  }
  /* Many undumps are host-STACK temporaries (sentinel probes like
   * `undump_int(x); if (x != MEM_TOP) ...`). Rewinding those into a dead
   * frame would corrupt the stack; real globals, fmt arrays and the pools
   * all live far from the engine thread's stack window captured at run
   * start. Temporaries are re-derived by the replay anyway. */
  uintptr_t a = (uintptr_t) p;
  if (a >= g_stack_lo && a < g_stack_hi) {
    return;
  }
  /* Coalesce adjacent runs (the web2c undump stream is largely sequential
   * over global arrays) to keep the table small. */
  if (g_scalar_count > 0) {
    oxi_scalar_reg *last = &g_scalar_regs[g_scalar_count - 1];
    if (last->p + last->n == (const unsigned char *)p) {
      last->n += (uint64_t)n;
      return;
    }
  }
  if (g_scalar_count >= OXI_SCALAR_MAX) {
    g_scalar_overflow = 1; /* refuse the whole rewind rather than half-apply */
    return;
  }
  g_scalar_regs[g_scalar_count].p = (const unsigned char *)p;
  g_scalar_regs[g_scalar_count].n = (uint64_t)n;
  g_scalar_count++;
}

/* True when [r->p, r->p + r->n) intersects one of the five live pool
 * arrays. Those blocks are captured/restored by the fence machinery using
 * LIVE bases (the arrays realloc - and therefore MOVE - during a pass),
 * so replaying their recorded stale addresses would write freed memory
 * and corrupt the heap. They are compacted out at capture time. */
static int oxi_reg_overlaps_pool(const oxi_scalar_reg *r, uint64_t sizes[5],
                                 const void *bases[5]) {
  for (int i = 0; i < 5; i++) {
    if (sizes[i] == 0) {
      continue;
    }
    const unsigned char *b = (const unsigned char *)bases[i];
    if (r->p < b + sizes[i] && b < r->p + r->n) {
      return 1;
    }
  }
  return 0;
}

static void oxi_scalars_capture(void) {
  /* Compact out pool-overlapping regions first (fence restore owns those
   * bytes via live bases); the rest are static globals and never-realloc'd
   * fmt arrays, whose addresses stay valid until the deferred cleanup. */
  uint64_t pool_sizes[5];
  const void *pool_bases[5];
  oxi_snap_sizes(pool_sizes, pool_bases);
  int kept = 0;
  for (int i = 0; i < g_scalar_count; i++) {
    if (!oxi_reg_overlaps_pool(&g_scalar_regs[i], pool_sizes, pool_bases)) {
      g_scalar_regs[kept++] = g_scalar_regs[i];
    }
  }
  g_scalar_count = kept;
  uint64_t total = 0;
  for (int i = 0; i < g_scalar_count; i++) {
    total += g_scalar_regs[i].n;
  }
  free(g_scalar_buf);
  g_scalar_buf = NULL;
  g_scalar_data = 0;
  uint8_t *buf = (uint8_t *)malloc((size_t)total);
  if (buf == NULL) {
    return;
  }
  uint8_t *p = buf;
  for (int i = 0; i < g_scalar_count; i++) {
    memcpy(p, g_scalar_regs[i].p, (size_t)g_scalar_regs[i].n);
    p += g_scalar_regs[i].n;
  }
  g_scalar_buf = buf;
  g_scalar_data = total;
  g_scalars_recording = 0; /* the fmt is fully loaded past this point */
  fprintf(stderr, "[oxi] S0 capture: regs=%d bytes=%llu\n", g_scalar_count,
          (unsigned long long) total);
  fflush(stderr);
}

static int oxi_scalars_restore(void) {
  if (!g_scalar_buf || g_scalar_overflow) {
    return -1; /* never half-apply a truncated rewind set */
  }
  const uint8_t *p = g_scalar_buf;
  for (int i = 0; i < g_scalar_count; i++) {
    if (g_scalar_regs[i].n > g_scalar_data - (uint64_t)(p - g_scalar_buf)) {
      return -1;
    }
    memcpy((void *)g_scalar_regs[i].p, p, (size_t)g_scalar_regs[i].n);
    p += g_scalar_regs[i].n;
  }
  return 0;
}

void oxipresso_xetex_enable_resident_passes(void) { g_resident_enabled = 1; }

/* Tear the resident mode down after its session died: no more fences, and
 * any pending arm is dropped so a fallback full restart runs fence-less. */
void oxipresso_xetex_disable_resident_passes(void) {
  g_resident_enabled = 0;
  g_fence_request = 0;
  g_fence_park_mode = 0;
  g_fence_tp_active = 0;
  g_fence_edit_pending = 0;
}

uint64_t oxipresso_xetex_fence_park_kind(void) {
  return (uint64_t)g_fence_park_kind;
}

void oxipresso_resident_capture(void) {
  if (!g_resident_enabled || !active_session || !active_session->callbacks) {
    return;
  }
  /* The runtime input stack is NOT in the fmt undump set (only a loc
   * special-case), so the scalar recorder never sees it - and pass-1's
   * TERMINAL input state (measured: cur_input.loc ~5e6, limit=1, files
   * closed) leaked into pass-2, whose start_input then read zero bytes
   * and the token walk spun on the torn state. Register the whole stack
   * + top frame + open count into the rewind set before capture. */
  oxipresso_undump_record(&cur_input, sizeof(cur_input));
  oxipresso_undump_record(&in_open, sizeof(in_open));
  oxipresso_undump_record(&input_ptr, sizeof(input_ptr));
  oxipresso_undump_record(&param_ptr, sizeof(param_ptr));
  oxipresso_undump_record(&max_param_stack, sizeof(max_param_stack));
  /* fmt scalars the host restores via its STACK TEMP (`undump_int(x);
   * var = x;`): do_undump records the temp's stack address (filtered), so
   * these globals are invisible to the recorder and pass-1 terminal values
   * leak. Measured casualties: `rover` (pass-2 get_node walked the S0 free
   * list from pass-1's terminal rover -> infinite loop at \immediate\write)
   * and `avail` (one-word list head every get_avail starts from). */
  oxipresso_undump_record(&rover, sizeof(rover));
  oxipresso_undump_record(&lo_mem_max, sizeof(lo_mem_max));
  oxipresso_undump_record(&hi_mem_min, sizeof(hi_mem_min));
  oxipresso_undump_record(&avail, sizeof(avail));
  oxipresso_undump_record(&hash_used, sizeof(hash_used));
  oxipresso_undump_record(&font_ptr, sizeof(font_ptr));
  oxipresso_undump_record(&par_loc, sizeof(par_loc));
  oxipresso_undump_record(&write_loc, sizeof(write_loc));
  oxipresso_undump_record(&hyph_count, sizeof(hyph_count));
  oxipresso_undump_record(&hyph_next, sizeof(hyph_next));
  oxipresso_undump_record(&hyph_start, sizeof(hyph_start));
  /* Measured via the pass-2 mirror dump: `\immediate\write`'s balanced
   * scan ran away ("Paragraph ended before \@parse@version was
   * complete") because scan_toks balances braces on the GLOBAL
   * align_state, whose pass-1 terminal value leaked (baseline showed
   * align=1000000 mid-document). scanner_status guards the same paths. */
  oxipresso_undump_record(&align_state, sizeof(align_state));
  oxipresso_undump_record(&scanner_status, sizeof(scanner_status));
  /* Job/log name cluster (measured via the pass-2 mirror: "Output written
   * on ??? (1 page, 3084 bytes)" - the XDV was WRITTEN but to a torn
   * path, because start_input only sets job_name when it is ZERO and
   * pass-1 had already set it to a str number that no longer exists in
   * the rewound string pool). */
  oxipresso_undump_record(&job_name, sizeof(job_name));
  oxipresso_undump_record(&log_opened, sizeof(log_opened));
  oxipresso_undump_record(&texmf_log_name, sizeof(texmf_log_name));
  /* Output-routing and scanner-flag cluster (measured: pass-2 failed at
   * \documentclass with "tokens_to_string() called while selector =
   * new_string" - the selector and scanner flags are runtime state the
   * host pre-phase initializes but nothing rewinds). */
  oxipresso_undump_record(&selector, sizeof(selector));
  oxipresso_undump_record(&force_eof, sizeof(force_eof));
  oxipresso_undump_record(&name_in_progress, sizeof(name_in_progress));
  oxipresso_undump_record(&long_state, sizeof(long_state));
  oxipresso_undump_record(&used_tectonic_coda_tokens,
                          sizeof(used_tectonic_coda_tokens));
  /* Scanner result globals (measured: pass-2's stream-number scan for
   * \write said "A number should have been here; I inserted 0" - the scan
   * machinery's result state leaked from pass-1). */
  oxipresso_undump_record(&cur_val, sizeof(cur_val));
  oxipresso_undump_record(&cur_val1, sizeof(cur_val1));
  oxipresso_undump_record(&cur_val_level, sizeof(cur_val_level));
  oxipresso_undump_record(&radix, sizeof(radix));
  oxipresso_undump_record(&cur_order, sizeof(cur_order));
  /* SYSTEMATIC SWEEP (r256c): every non-pointer extern global of the engine
   * (generated from xetex-xetexd.h). The whack-a-mole registrations above
   * are subsumed by this block; duplicates are harmless (same value
   * rewritten). Pointer-typed globals are deliberately excluded - their
   * targets are either stable fmt-load allocations (restoring the pointer
   * is a no-op) or the realloc'd pools, which the fence machinery restores
   * through LIVE bases.
   */  oxipresso_undump_record(&shell_escape_enabled, sizeof(shell_escape_enabled));
  oxipresso_undump_record(&bad, sizeof(bad));
  oxipresso_undump_record(&name_length, sizeof(name_length));
  oxipresso_undump_record(&name_length16, sizeof(name_length16));
  oxipresso_undump_record(&first, sizeof(first));
  oxipresso_undump_record(&last, sizeof(last));
  oxipresso_undump_record(&max_buf_stack, sizeof(max_buf_stack));
  oxipresso_undump_record(&in_initex_mode, sizeof(in_initex_mode));
  oxipresso_undump_record(&error_line, sizeof(error_line));
  oxipresso_undump_record(&half_error_line, sizeof(half_error_line));
  oxipresso_undump_record(&max_print_line, sizeof(max_print_line));
  oxipresso_undump_record(&max_strings, sizeof(max_strings));
  oxipresso_undump_record(&strings_free, sizeof(strings_free));
  oxipresso_undump_record(&string_vacancies, sizeof(string_vacancies));
  oxipresso_undump_record(&pool_size, sizeof(pool_size));
  oxipresso_undump_record(&pool_free, sizeof(pool_free));
  oxipresso_undump_record(&font_mem_size, sizeof(font_mem_size));
  oxipresso_undump_record(&font_max, sizeof(font_max));
  oxipresso_undump_record(&hyph_size, sizeof(hyph_size));
  oxipresso_undump_record(&trie_size, sizeof(trie_size));
  oxipresso_undump_record(&buf_size, sizeof(buf_size));
  oxipresso_undump_record(&stack_size, sizeof(stack_size));
  oxipresso_undump_record(&max_in_open, sizeof(max_in_open));
  oxipresso_undump_record(&param_size, sizeof(param_size));
  oxipresso_undump_record(&nest_size, sizeof(nest_size));
  oxipresso_undump_record(&save_size, sizeof(save_size));
  oxipresso_undump_record(&expand_depth, sizeof(expand_depth));
  oxipresso_undump_record(&file_line_error_style_p, sizeof(file_line_error_style_p));
  oxipresso_undump_record(&halt_on_error_p, sizeof(halt_on_error_p));
  oxipresso_undump_record(&quoted_filename, sizeof(quoted_filename));
  oxipresso_undump_record(&insert_src_special_auto, sizeof(insert_src_special_auto));
  oxipresso_undump_record(&insert_src_special_every_par, sizeof(insert_src_special_every_par));
  oxipresso_undump_record(&insert_src_special_every_math, sizeof(insert_src_special_every_math));
  oxipresso_undump_record(&insert_src_special_every_vbox, sizeof(insert_src_special_every_vbox));
  oxipresso_undump_record(&pool_ptr, sizeof(pool_ptr));
  oxipresso_undump_record(&str_ptr, sizeof(str_ptr));
  oxipresso_undump_record(&init_pool_ptr, sizeof(init_pool_ptr));
  oxipresso_undump_record(&init_str_ptr, sizeof(init_str_ptr));
  oxipresso_undump_record(&selector, sizeof(selector));
  oxipresso_undump_record(&tally, sizeof(tally));
  oxipresso_undump_record(&term_offset, sizeof(term_offset));
  oxipresso_undump_record(&file_offset, sizeof(file_offset));
  oxipresso_undump_record(&trick_count, sizeof(trick_count));
  oxipresso_undump_record(&first_count, sizeof(first_count));
  oxipresso_undump_record(&doing_special, sizeof(doing_special));
  oxipresso_undump_record(&native_text_size, sizeof(native_text_size));
  oxipresso_undump_record(&native_len, sizeof(native_len));
  oxipresso_undump_record(&save_native_len, sizeof(save_native_len));
  oxipresso_undump_record(&interaction, sizeof(interaction));
  oxipresso_undump_record(&deletions_allowed, sizeof(deletions_allowed));
  oxipresso_undump_record(&set_box_allowed, sizeof(set_box_allowed));
  oxipresso_undump_record(&history, sizeof(history));
  oxipresso_undump_record(&error_count, sizeof(error_count));
  oxipresso_undump_record(&help_ptr, sizeof(help_ptr));
  oxipresso_undump_record(&use_err_help, sizeof(use_err_help));
  oxipresso_undump_record(&arith_error, sizeof(arith_error));
  oxipresso_undump_record(&tex_remainder, sizeof(tex_remainder));
  oxipresso_undump_record(&j_random, sizeof(j_random));
  oxipresso_undump_record(&random_seed, sizeof(random_seed));
  oxipresso_undump_record(&temp_ptr, sizeof(temp_ptr));
  oxipresso_undump_record(&lo_mem_max, sizeof(lo_mem_max));
  oxipresso_undump_record(&hi_mem_min, sizeof(hi_mem_min));
  oxipresso_undump_record(&dyn_used, sizeof(dyn_used));
  oxipresso_undump_record(&avail, sizeof(avail));
  oxipresso_undump_record(&mem_end, sizeof(mem_end));
  oxipresso_undump_record(&rover, sizeof(rover));
  oxipresso_undump_record(&last_leftmost_char, sizeof(last_leftmost_char));
  oxipresso_undump_record(&last_rightmost_char, sizeof(last_rightmost_char));
  oxipresso_undump_record(&hlist_stack_level, sizeof(hlist_stack_level));
  oxipresso_undump_record(&first_p, sizeof(first_p));
  oxipresso_undump_record(&global_prev_p, sizeof(global_prev_p));
  oxipresso_undump_record(&font_in_short_display, sizeof(font_in_short_display));
  oxipresso_undump_record(&depth_threshold, sizeof(depth_threshold));
  oxipresso_undump_record(&breadth_max, sizeof(breadth_max));
  oxipresso_undump_record(&nest_ptr, sizeof(nest_ptr));
  oxipresso_undump_record(&max_nest_stack, sizeof(max_nest_stack));
  oxipresso_undump_record(&cur_list, sizeof(cur_list));
  oxipresso_undump_record(&shown_mode, sizeof(shown_mode));
  oxipresso_undump_record(&old_setting, sizeof(old_setting));
  oxipresso_undump_record(&hash_used, sizeof(hash_used));
  oxipresso_undump_record(&hash_extra, sizeof(hash_extra));
  oxipresso_undump_record(&hash_top, sizeof(hash_top));
  oxipresso_undump_record(&eqtb_top, sizeof(eqtb_top));
  oxipresso_undump_record(&hash_high, sizeof(hash_high));
  oxipresso_undump_record(&no_new_control_sequence, sizeof(no_new_control_sequence));
  oxipresso_undump_record(&cs_count, sizeof(cs_count));
  oxipresso_undump_record(&prim_used, sizeof(prim_used));
  oxipresso_undump_record(&save_ptr, sizeof(save_ptr));
  oxipresso_undump_record(&max_save_stack, sizeof(max_save_stack));
  oxipresso_undump_record(&cur_level, sizeof(cur_level));
  oxipresso_undump_record(&cur_group, sizeof(cur_group));
  oxipresso_undump_record(&cur_boundary, sizeof(cur_boundary));
  oxipresso_undump_record(&mag_set, sizeof(mag_set));
  oxipresso_undump_record(&cur_cmd, sizeof(cur_cmd));
  oxipresso_undump_record(&cur_chr, sizeof(cur_chr));
  oxipresso_undump_record(&cur_cs, sizeof(cur_cs));
  oxipresso_undump_record(&cur_tok, sizeof(cur_tok));
  oxipresso_undump_record(&input_ptr, sizeof(input_ptr));
  oxipresso_undump_record(&max_in_stack, sizeof(max_in_stack));
  oxipresso_undump_record(&cur_input, sizeof(cur_input));
  oxipresso_undump_record(&in_open, sizeof(in_open));
  oxipresso_undump_record(&open_parens, sizeof(open_parens));
  oxipresso_undump_record(&line, sizeof(line));
  oxipresso_undump_record(&scanner_status, sizeof(scanner_status));
  oxipresso_undump_record(&warning_index, sizeof(warning_index));
  oxipresso_undump_record(&def_ref, sizeof(def_ref));
  oxipresso_undump_record(&param_ptr, sizeof(param_ptr));
  oxipresso_undump_record(&max_param_stack, sizeof(max_param_stack));
  oxipresso_undump_record(&align_state, sizeof(align_state));
  oxipresso_undump_record(&base_ptr, sizeof(base_ptr));
  oxipresso_undump_record(&par_loc, sizeof(par_loc));
  oxipresso_undump_record(&par_token, sizeof(par_token));
  oxipresso_undump_record(&force_eof, sizeof(force_eof));
  oxipresso_undump_record(&expand_depth_count, sizeof(expand_depth_count));
  oxipresso_undump_record(&is_in_csname, sizeof(is_in_csname));
  oxipresso_undump_record(&long_state, sizeof(long_state));
  oxipresso_undump_record(&cur_val, sizeof(cur_val));
  oxipresso_undump_record(&cur_val1, sizeof(cur_val1));
  oxipresso_undump_record(&cur_val_level, sizeof(cur_val_level));
  oxipresso_undump_record(&radix, sizeof(radix));
  oxipresso_undump_record(&cur_order, sizeof(cur_order));
  oxipresso_undump_record(&cond_ptr, sizeof(cond_ptr));
  oxipresso_undump_record(&if_limit, sizeof(if_limit));
  oxipresso_undump_record(&cur_if, sizeof(cur_if));
  oxipresso_undump_record(&if_line, sizeof(if_line));
  oxipresso_undump_record(&skip_line, sizeof(skip_line));
  oxipresso_undump_record(&cur_name, sizeof(cur_name));
  oxipresso_undump_record(&cur_area, sizeof(cur_area));
  oxipresso_undump_record(&cur_ext, sizeof(cur_ext));
  oxipresso_undump_record(&area_delimiter, sizeof(area_delimiter));
  oxipresso_undump_record(&ext_delimiter, sizeof(ext_delimiter));
  oxipresso_undump_record(&file_name_quote_char, sizeof(file_name_quote_char));
  oxipresso_undump_record(&format_default_length, sizeof(format_default_length));
  oxipresso_undump_record(&name_in_progress, sizeof(name_in_progress));
  oxipresso_undump_record(&job_name, sizeof(job_name));
  oxipresso_undump_record(&log_opened, sizeof(log_opened));
  oxipresso_undump_record(&texmf_log_name, sizeof(texmf_log_name));
  oxipresso_undump_record(&fmem_ptr, sizeof(fmem_ptr));
  oxipresso_undump_record(&font_ptr, sizeof(font_ptr));
  oxipresso_undump_record(&loaded_font_flags, sizeof(loaded_font_flags));
  oxipresso_undump_record(&loaded_font_letter_space, sizeof(loaded_font_letter_space));
  oxipresso_undump_record(&null_character, sizeof(null_character));
  oxipresso_undump_record(&total_pages, sizeof(total_pages));
  oxipresso_undump_record(&max_v, sizeof(max_v));
  oxipresso_undump_record(&max_h, sizeof(max_h));
  oxipresso_undump_record(&max_push, sizeof(max_push));
  oxipresso_undump_record(&last_bop, sizeof(last_bop));
  oxipresso_undump_record(&dead_cycles, sizeof(dead_cycles));
  oxipresso_undump_record(&doing_leaders, sizeof(doing_leaders));
  oxipresso_undump_record(&rule_wd, sizeof(rule_wd));
  oxipresso_undump_record(&epochseconds, sizeof(epochseconds));
  oxipresso_undump_record(&microseconds, sizeof(microseconds));
  oxipresso_undump_record(&last_badness, sizeof(last_badness));
  oxipresso_undump_record(&adjust_tail, sizeof(adjust_tail));
  oxipresso_undump_record(&pre_adjust_tail, sizeof(pre_adjust_tail));
  oxipresso_undump_record(&pack_begin_line, sizeof(pack_begin_line));
  oxipresso_undump_record(&empty, sizeof(empty));
  oxipresso_undump_record(&cur_f, sizeof(cur_f));
  oxipresso_undump_record(&cur_c, sizeof(cur_c));
  oxipresso_undump_record(&cur_i, sizeof(cur_i));
  oxipresso_undump_record(&cur_align, sizeof(cur_align));
  oxipresso_undump_record(&cur_span, sizeof(cur_span));
  oxipresso_undump_record(&cur_loop, sizeof(cur_loop));
  oxipresso_undump_record(&align_ptr, sizeof(align_ptr));
  oxipresso_undump_record(&cur_tail, sizeof(cur_tail));
  oxipresso_undump_record(&cur_pre_tail, sizeof(cur_pre_tail));
  oxipresso_undump_record(&just_box, sizeof(just_box));
  oxipresso_undump_record(&hf, sizeof(hf));
  oxipresso_undump_record(&cur_lang, sizeof(cur_lang));
  oxipresso_undump_record(&max_hyph_char, sizeof(max_hyph_char));
  oxipresso_undump_record(&init_list, sizeof(init_list));
  oxipresso_undump_record(&init_lig, sizeof(init_lig));
  oxipresso_undump_record(&init_lft, sizeof(init_lft));
  oxipresso_undump_record(&hyphen_passed, sizeof(hyphen_passed));
  oxipresso_undump_record(&cur_r, sizeof(cur_r));
  oxipresso_undump_record(&cur_q, sizeof(cur_q));
  oxipresso_undump_record(&lig_stack, sizeof(lig_stack));
  oxipresso_undump_record(&ligature_present, sizeof(ligature_present));
  oxipresso_undump_record(&rt_hit, sizeof(rt_hit));
  oxipresso_undump_record(&hyph_count, sizeof(hyph_count));
  oxipresso_undump_record(&hyph_next, sizeof(hyph_next));
  oxipresso_undump_record(&trie_op_ptr, sizeof(trie_op_ptr));
  oxipresso_undump_record(&max_op_used, sizeof(max_op_used));
  oxipresso_undump_record(&trie_ptr, sizeof(trie_ptr));
  oxipresso_undump_record(&trie_max, sizeof(trie_max));
  oxipresso_undump_record(&trie_not_ready, sizeof(trie_not_ready));
  oxipresso_undump_record(&best_height_plus_depth, sizeof(best_height_plus_depth));
  oxipresso_undump_record(&page_tail, sizeof(page_tail));
  oxipresso_undump_record(&page_contents, sizeof(page_contents));
  oxipresso_undump_record(&last_glue, sizeof(last_glue));
  oxipresso_undump_record(&last_penalty, sizeof(last_penalty));
  oxipresso_undump_record(&last_kern, sizeof(last_kern));
  oxipresso_undump_record(&last_node_type, sizeof(last_node_type));
  oxipresso_undump_record(&insert_penalties, sizeof(insert_penalties));
  oxipresso_undump_record(&output_active, sizeof(output_active));
  oxipresso_undump_record(&main_f, sizeof(main_f));
  oxipresso_undump_record(&main_i, sizeof(main_i));
  oxipresso_undump_record(&main_j, sizeof(main_j));
  oxipresso_undump_record(&main_k, sizeof(main_k));
  oxipresso_undump_record(&main_p, sizeof(main_p));
  oxipresso_undump_record(&main_ppp, sizeof(main_ppp));
  oxipresso_undump_record(&main_h, sizeof(main_h));
  oxipresso_undump_record(&is_hyph, sizeof(is_hyph));
  oxipresso_undump_record(&space_class, sizeof(space_class));
  oxipresso_undump_record(&prev_class, sizeof(prev_class));
  oxipresso_undump_record(&main_s, sizeof(main_s));
  oxipresso_undump_record(&bchar, sizeof(bchar));
  oxipresso_undump_record(&false_bchar, sizeof(false_bchar));
  oxipresso_undump_record(&cancel_boundary, sizeof(cancel_boundary));
  oxipresso_undump_record(&ins_disc, sizeof(ins_disc));
  oxipresso_undump_record(&cur_box, sizeof(cur_box));
  oxipresso_undump_record(&after_token, sizeof(after_token));
  oxipresso_undump_record(&long_help_seen, sizeof(long_help_seen));
  oxipresso_undump_record(&format_ident, sizeof(format_ident));
  oxipresso_undump_record(&write_loc, sizeof(write_loc));
  oxipresso_undump_record(&cur_page_width, sizeof(cur_page_width));
  oxipresso_undump_record(&cur_page_height, sizeof(cur_page_height));
  oxipresso_undump_record(&cur_h_offset, sizeof(cur_h_offset));
  oxipresso_undump_record(&cur_v_offset, sizeof(cur_v_offset));
  oxipresso_undump_record(&pdf_last_x_pos, sizeof(pdf_last_x_pos));
  oxipresso_undump_record(&pdf_last_y_pos, sizeof(pdf_last_y_pos));
  oxipresso_undump_record(&LR_ptr, sizeof(LR_ptr));
  oxipresso_undump_record(&LR_problems, sizeof(LR_problems));
  oxipresso_undump_record(&cur_dir, sizeof(cur_dir));
  oxipresso_undump_record(&pseudo_files, sizeof(pseudo_files));
  oxipresso_undump_record(&max_reg_num, sizeof(max_reg_num));
  oxipresso_undump_record(&cur_ptr, sizeof(cur_ptr));
  oxipresso_undump_record(&sa_null, sizeof(sa_null));
  oxipresso_undump_record(&sa_chain, sizeof(sa_chain));
  oxipresso_undump_record(&sa_level, sizeof(sa_level));
  oxipresso_undump_record(&hyph_start, sizeof(hyph_start));
  oxipresso_undump_record(&hyph_index, sizeof(hyph_index));
  oxipresso_undump_record(&edit_name_start, sizeof(edit_name_start));
  oxipresso_undump_record(&stop_at_space, sizeof(stop_at_space));
  oxipresso_undump_record(&native_font_type_flag, sizeof(native_font_type_flag));
  oxipresso_undump_record(&xtx_ligature_present, sizeof(xtx_ligature_present));
  oxipresso_undump_record(&delta, sizeof(delta));
  oxipresso_undump_record(&synctex_enabled, sizeof(synctex_enabled));
  oxipresso_undump_record(&synctex_use_gz, sizeof(synctex_use_gz));
  oxipresso_undump_record(&synctex_texpresso_extension, sizeof(synctex_texpresso_extension));
  oxipresso_undump_record(&used_tectonic_coda_tokens, sizeof(used_tectonic_coda_tokens));
  oxipresso_undump_record(&semantic_pagination_enabled, sizeof(semantic_pagination_enabled));
  oxipresso_undump_record(&gave_char_warning_help, sizeof(gave_char_warning_help));

  if (input_stack != NULL && stack_size > 0) {
    oxipresso_undump_record(input_stack,
                            sizeof(input_state_t) * (size_t)stack_size);
  }
  /* Macro-parameter stack and the per-input-slot arrays: all fmt-load-time
   * fixed allocations (never realloc'd), pass-1 terminal values otherwise
   * leak - measured: pass-2 saw param_ptr=1 and the OUT_PARAM branch
   * restart-looped on a torn param_stack entry (get_next #1782497 hung
   * without ever returning, invisible to every entry probe). */
  if (param_stack != NULL && param_size > 0) {
    oxipresso_undump_record(param_stack,
                            sizeof(int32_t) * (size_t)param_size);
  }
  if (eof_seen != NULL && max_in_open > 0) {
    oxipresso_undump_record(eof_seen, sizeof(bool) * (size_t)max_in_open);
  }
  if (grp_stack != NULL && max_in_open > 0) {
    oxipresso_undump_record(grp_stack,
                            sizeof(save_pointer) * (size_t)max_in_open);
  }
  if (if_stack != NULL && max_in_open > 0) {
    oxipresso_undump_record(if_stack,
                            sizeof(int32_t) * (size_t)max_in_open);
  }
  /* Remaining heap-allocated arrays: buffer (line input), nest (nesting
   * state), line/source stacks, and explicit full-coverage eqtb/hash.
   * These are fmt-load-time allocations with stable addresses; their
   * content is mutated during each pass but never recorded by do_undump
   * (buffer/nest/line_stack are runtime-only, eqtb/hash undump only
   * covers a subrange). */
  if (buffer != NULL && buf_size > 0) {
    oxipresso_undump_record(buffer, sizeof(UTF16_code) * (size_t)buf_size);
  }
  if (nest != NULL && nest_size > 0) {
    oxipresso_undump_record(nest,
                            sizeof(list_state_record) * (size_t)nest_size);
  }
  if (line_stack != NULL && max_in_open > 0) {
    oxipresso_undump_record(line_stack,
                            sizeof(int32_t) * (size_t)max_in_open);
  }
  if (source_filename_stack != NULL && max_in_open > 0) {
    oxipresso_undump_record(source_filename_stack,
                            sizeof(str_number) * (size_t)max_in_open);
  }
  if (full_source_filename_stack != NULL && max_in_open > 0) {
    oxipresso_undump_record(full_source_filename_stack,
                            sizeof(str_number) * (size_t)max_in_open);
  }
  if (eqtb != NULL) {
    oxipresso_undump_record(eqtb, sizeof(memory_word) * (size_t)(eqtb_top + 1));
  }
  /* Hash table: the collision chain links live in the yhash allocation,
   * NOT in mem. Pass-1 creates new CS entries whose chain links are ABOVE
   * the format-load undump range. Without restoring these, pass-2's CS
   * lookups walk stale chains, giving the SAME CS name a DIFFERENT index
   * (measured: __hook_next class/article/before at cs 8962232 vs
   * 8962234), shifting macro_call entry by one token. */
  if (hash != NULL && hash_top >= HASH_BASE) {
    oxipresso_undump_record(
        &hash[HASH_BASE],
        sizeof(b32x2) * (size_t)(hash_top - HASH_BASE + 1));
  }
  fprintf(stderr,
          "[oxi] istack probe: regs=%d cur@%p loc=%d limit=%d st=%d "
          "in_open@%p val=%d stack@%p lo=%p hi=%p\n",
          g_scalar_count, (void *)&cur_input, (int)cur_input.loc,
          (int)cur_input.limit, (int)cur_input.state, (void *)&in_open,
          (int)in_open, (void *)input_stack, (void *)g_stack_lo,
          (void *)g_stack_hi);
  fflush(stderr);
  oxi_scalars_capture();
  oxi_fence_capture_s0();
  g_fence_park_kind = 1;
  if (active_session->callbacks->fence) {
    int cmd = active_session->callbacks->fence(
        active_session->callbacks->userdata);
    /* KEY EXPERIMENT (r256d): restore here too. With the COMPLETE rewind
     * set this is an identity (we sit at S0), and if pass-1 THEN fails the
     * same way pass-2 does, the restore itself is unfaithful - localizing
     * the bug to restore fidelity instead of pass-1 mutations. */
    if (cmd == 2) {
      if (oxi_fence_restore_s0(1) != 0 || oxi_scalars_restore() != 0) {
        fprintf(stderr, "[oxi] park-1 restore FAILED\n");
        fflush(stderr);
      }
    }
  }
  g_fence_park_kind = 0;
}

int oxipresso_resident_park(void) {
  if (!g_resident_enabled || !active_session || !active_session->callbacks ||
      !active_session->callbacks->fence) {
    return 0;
  }
  g_res_parks++;
  fprintf(stderr, "[oxi] resident_park entry #%llu reads=%llu\n",
          (unsigned long long) g_res_parks, (unsigned long long) g_res_reads);
  fflush(stderr);
  g_fence_park_kind = 1;
  int cmd = active_session->callbacks->fence(
      active_session->callbacks->userdata);
  g_fence_park_kind = 0;
  if ((cmd == 2 || cmd == 5) && oxi_fence_restore_s0(1) == 0 &&
      oxi_scalars_restore() == 0) {
    if (cmd == 5) {
      /* Background replay: re-run the pass purely to reach the target
       * fence and park there as the idle checkpoint. The mid-run fence
       * parks at the first target-file read at/beyond the target cursor;
       * the pass is speculative (same content as the artifact already
       * shipped) and never reaches its own boundary while parked. */
      g_fence_tp_active = 1;
    }
    return 1;
  }
  g_resident_enabled = 0; /* finish/timeout/mismatch: normal single-run mode */
  return 0;
}

int oxipresso_xetex_run(const oxi_xetex_config *config,
                        const oxi_xetex_callbacks *callbacks,
                        oxi_xetex_result *result) {
  if (!config || !callbacks || !result) {
    return -1;
  }
  if (active_session) {
    return -2;
  }

  oxi_session session;
  memset(&session, 0, sizeof(session));
  session.config = config;
  session.callbacks = callbacks;
  session.root_name = copy_slice(config->root_name, config->root_name_len);
  session.format_path = copy_slice(config->format_path, config->format_path_len);
  session.primary_name = copy_slice(config->primary_name, config->primary_name_len);
  if (!session.root_name || !session.format_path || !session.primary_name) {
    free(session.root_name);
    free(session.format_path);
    free(session.primary_name);
    return -3;
  }

  active_session = &session;
  session.abort_active = 1;
  /* Resident mode: start recording the do_undump target stream so the fmt
   * load's complete scalar/array surface is captured at S0 (see
   * oxipresso_undump_record). Cleared by oxi_scalars_capture at S0. */
  if (g_resident_enabled) {
    uintptr_t frame = (uintptr_t) &session;
    g_stack_lo = frame - (8u << 20); /* below: engine frames + guard */
    g_stack_hi = frame + (64u << 10); /* above: only our own callers */
    g_scalar_count = 0;
    g_scalar_overflow = 0;
    g_scalars_recording = 1;
  }
  int status = 3;
  if (setjmp(session.abort_jump) == 0) {
    tt_xetex_set_int_variable("in_initex_mode", config->in_initex_mode ? 1 : 0);
    /* Format bootstrap should stop at the first error; live document runs
     * keep going so diagnostics stream to the editor. */
    tt_xetex_set_int_variable("halt_on_error_p", config->in_initex_mode ? 1 : 0);
    /* Plain-text SyncTeX sidecar (oxipresso-synctex decodes both plain and
     * gzip, but the plain stream needs no extra flag plumbing). */
    tt_xetex_set_int_variable("synctex_enabled", config->synctex_enabled ? 1 : 0);
    tt_xetex_set_int_variable("synctex_use_gz", 0);
    /* TeXpresso's always-on extension (main.c sets it unconditionally): it makes
     * synctex_end_file_reading emit `/<tag>` closed-input records into the
     * sidecar, matching the `.synctex` byte layout TeXpresso produces. Our
     * parser ignores the `/` records (they are advisory input-closure markers). */
    tt_xetex_set_int_variable("synctex_texpresso_extension", 1);
    status = tt_engine_xetex_main(session.format_path,
                                  config->in_initex_mode && session.primary_name[0]
                                      ? session.primary_name
                                      : session.root_name,
                                  config->build_date);
  } else {
    status = 3;
  }
  session.abort_active = 0;

  result->status = status;
  result->page_count = 0;

  free(session.root_name);
  free(session.format_path);
  free(session.primary_name);
  free(session.last_input_path);
  active_session = NULL;
  return 0;
}
