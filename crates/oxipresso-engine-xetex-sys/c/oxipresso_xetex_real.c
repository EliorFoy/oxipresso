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
static void oxi_fence_capture(void) {
  /* Fresh capture per arm: the previous checkpoint buffer is superseded. */
  free(g_fence_buf);
  g_fence_buf = NULL;
  g_fence_len = 0;
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
  uint64_t sizes[5];
  const void *bases[5];
  uint64_t need = oxi_snap_sizes(sizes, bases);
  if (!g_fence_buf) {
    return -1;
  }
  const uint64_t *hdr = (const uint64_t *)g_fence_buf;
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
  if (g_fence_len != total) {
    return -1; /* buffer must be exactly header + captured blocks */
  }
  /* Restore the bump cursors (hdr slots 6 and 8..11; slots 5/7 are the
   * retired eqtb pair). Inside a frozen mid-run window these are already
   * the captured values; across a completed pass they are far ahead and
   * MUST rewind, or the restored prefixes would be shadowed by live junk. */
  mem_end = (int32_t)hdr[6];
  str_ptr = (str_number)hdr[8];
  pool_ptr = (pool_pointer)hdr[9];
  save_ptr = (int32_t)hdr[10];
  fmem_ptr = (font_index)hdr[11];
  const uint8_t *p = g_fence_buf + 12 * (uint64_t)sizeof(uint64_t);
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

uint64_t oxipresso_xetex_fence_park_kind(void) {
  return (uint64_t)g_fence_park_kind;
}

void oxipresso_resident_capture(void) {
  if (!g_resident_enabled || !active_session || !active_session->callbacks) {
    return;
  }
  oxi_scalars_capture();
  oxi_fence_capture();
  g_fence_park_kind = 1;
  if (active_session->callbacks->fence) {
    int cmd = active_session->callbacks->fence(
        active_session->callbacks->userdata);
    /* NO restore here: we sit exactly AT S0, so writing the buffer back
     * onto the live pools would double-patch (and corrupt) them. A
     * controller edit applied during this park takes effect through the
     * VFS on pass 1's very first read; the buffer stays for the loop. */
    (void)cmd;
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
  if (cmd == 2 && oxi_fence_restore(1) == 0 && oxi_scalars_restore() == 0) {
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
