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
  uint64_t build_date;
  int stream_mode;
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
} oxi_xetex_callbacks;

typedef struct {
  const oxi_xetex_config *config;
  const oxi_xetex_callbacks *callbacks;
  char *root_name;
  char *format_path;
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

static oxi_session *active_session = NULL;
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
  if (active_session->callbacks->open_read(active_session->callbacks->userdata,
                                           requested_path,
                                           strlen(requested_path),
                                           (int)format,
                                           &handle) != 0) {
    return NULL;
  }

  ttbc_input_handle_t *input = (ttbc_input_handle_t *)calloc(1, sizeof(ttbc_input_handle_t));
  if (!input) {
    active_session->callbacks->close(active_session->callbacks->userdata, handle);
    return NULL;
  }
  input->handle = handle;
  input->ungot = -1;
  input->path = copy_slice(requested_path, strlen(requested_path));

  free(active_session->last_input_path);
  active_session->last_input_path = copy_slice(requested_path, strlen(requested_path));
  return input;
}

rust_input_handle_t ttstub_input_open(char const *path, ttbc_file_format format, int is_gz) {
  (void)is_gz;
  return input_open_path(path, format);
}

rust_input_handle_t ttstub_input_open_primary(void) {
  if (!active_session || !active_session->root_name) {
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
  if (!session.root_name || !session.format_path) {
    free(session.root_name);
    free(session.format_path);
    return -3;
  }

  active_session = &session;
  session.abort_active = 1;
  int status = 3;
  if (setjmp(session.abort_jump) == 0) {
    status = tt_engine_xetex_main(session.format_path, session.root_name, config->build_date);
  } else {
    status = 3;
  }
  session.abort_active = 0;

  result->status = status;
  result->page_count = 0;

  free(session.root_name);
  free(session.format_path);
  free(session.last_input_path);
  active_session = NULL;
  return 0;
}
