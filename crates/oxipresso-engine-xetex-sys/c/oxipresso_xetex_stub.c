#include <stdint.h>
#include <stddef.h>
#include <string.h>

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

int oxipresso_xetex_is_real(void) {
  return 0;
}

/* Pool snapshot is a real-engine capability; the stub exposes the same ABI
 * with no pools to read. */
uint64_t oxipresso_xetex_snapshot_bytes(void) { return 0; }

int64_t oxipresso_xetex_snapshot_capture(void *dst, uint64_t dst_len) {
  (void)dst;
  (void)dst_len;
  return -1;
}

void oxipresso_xetex_request_fence_snapshot(void) {}

uint64_t oxipresso_xetex_fence_snapshot_len(void) { return 0; }

uint64_t oxipresso_xetex_fence_snapshot_copy(void *dst, uint64_t dst_len) {
  (void)dst;
  (void)dst_len;
  return 0;
}

void oxipresso_xetex_request_fence_roundtrip(void) {}

uint64_t oxipresso_xetex_fence_roundtrip_fired(void) { return 0; }

void oxipresso_xetex_request_fence_restore(void) {}

uint64_t oxipresso_xetex_fence_restore_fired(void) { return 0; }

void oxipresso_xetex_request_fence_park(void) {}

void oxipresso_xetex_arm_fence_replay(void) {}

void oxipresso_xetex_enable_resident_passes(void) {}

uint64_t oxipresso_xetex_fence_park_kind(void) { return 0; }

void oxipresso_xetex_resident_debug_counts(uint64_t *parks, uint64_t *reads) {
  (void) parks;
  (void) reads;
}

int oxipresso_xetex_run(const oxi_xetex_config *config,
                        const oxi_xetex_callbacks *callbacks,
                        oxi_xetex_result *result) {
  uint32_t page_count = 0;

  if (config && callbacks && callbacks->open_read && callbacks->read &&
      callbacks->seen && callbacks->close) {
    uint32_t handle = 0;
    int opened = callbacks->open_read(callbacks->userdata,
                                      config->root_name,
                                      config->root_name_len,
                                      0,
                                      &handle);
    if (opened == 0) {
      const uint8_t *bytes = 0;
      size_t out_len = 0;
      size_t input_size = 0;
      if (callbacks->size) {
        callbacks->size(callbacks->userdata, handle, &input_size);
      }
      if (callbacks->read(callbacks->userdata, handle, 0, 4096, &bytes, &out_len) == 0) {
        size_t seen = input_size > out_len ? input_size : out_len;
        callbacks->seen(callbacks->userdata, handle, seen, 1);
        if (seen > 0) {
          page_count = 1;
        }
      }
      callbacks->close(callbacks->userdata, handle);
    }
  }

  if (callbacks && callbacks->open_write && callbacks->append && callbacks->close) {
    static const char stdout_path[] = "stdout";
    static const uint8_t message[] = "Oxipresso XeTeX FFI stub\n";
    uint32_t stdout_handle = 0;
    if (callbacks->open_write(callbacks->userdata,
                              stdout_path,
                              sizeof(stdout_path) - 1,
                              0,
                              &stdout_handle) == 0) {
      callbacks->append(callbacks->userdata,
                        stdout_handle,
                        message,
                        sizeof(message) - 1);
      if (callbacks->flush) {
        callbacks->flush(callbacks->userdata, stdout_handle);
      }
      callbacks->close(callbacks->userdata, stdout_handle);
    }
  }

  if (config && callbacks && callbacks->open_write && callbacks->append && callbacks->close) {
    char artifact_path[1024];
    size_t root_len = config->root_name_len;
    if (root_len > sizeof(artifact_path) - 5) {
      root_len = sizeof(artifact_path) - 5;
    }
    memcpy(artifact_path, config->root_name, root_len);
    size_t stem_len = root_len;
    for (size_t i = root_len; i > 0; i--) {
      char ch = artifact_path[i - 1];
      if (ch == '/' || ch == '\\') {
        break;
      }
      if (ch == '.') {
        stem_len = i - 1;
        break;
      }
    }
    memcpy(artifact_path + stem_len, ".xdv", 5);

    static const uint8_t artifact[] = "OXIPRESSO-STUB-XDV";
    uint32_t artifact_handle = 0;
    if (callbacks->open_write(callbacks->userdata,
                              artifact_path,
                              stem_len + 4,
                              0,
                              &artifact_handle) == 0) {
      callbacks->append(callbacks->userdata,
                        artifact_handle,
                        artifact,
                        sizeof(artifact) - 1);
      if (callbacks->flush) {
        callbacks->flush(callbacks->userdata, artifact_handle);
      }
      callbacks->close(callbacks->userdata, artifact_handle);
    }
  }

  if (callbacks && callbacks->diagnostic) {
    static const uint8_t message[] = "Oxipresso XeTeX FFI stub initialized";
    callbacks->diagnostic(callbacks->userdata, 0, message, sizeof(message) - 1);
  }

  if (result) {
    result->status = 0;
    result->page_count = page_count;
  }
  return 0;
}
