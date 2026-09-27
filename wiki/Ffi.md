# FFI

C bindings for ArchiveKit (`Headers/archivekit.h`, implemented in `src/ffi.rs`). All functions are `no_mangle extern "C"`. Format codes are `1 = zip`, `2 = gzip`, `3 = tar`, `4 = tar.gz`, `5 = app`; levels are `0 = none`, `1 = fastest`, `2 = balanced`, `3 = best`.

## Memory Rules

| Function family | Release with | Note |
|---|---|---|
| `archivekit_gzip_compress`, `archivekit_gzip_decompress`, `archivekit_tar_gzip_compress` | `archivekit_free_buffer(ptr, len)` | `len` comes from the call's `out_len` |
| `archivekit_list_names` | `archivekit_free_string(ptr)` | NULL pointers are ignored by both free functions |
| `archivekit_version`, `archivekit_last_error` | never freed | `last_error` is thread-local; NULL means no error |

## Version and Errors

### `archivekit_version`

```c
const char* archivekit_version(void);
```

| Return | Meaning |
|---|---|
| pointer | Version string, e.g. `"26.1.0"` (do NOT free) |

### `archivekit_last_error`

```c
const char* archivekit_last_error(void);
```

| Return | Meaning |
|---|---|
| pointer | Thread-local message of the most recent failure |
| NULL | Last call succeeded |

### `archivekit_free_string` / `archivekit_free_buffer`

```c
void archivekit_free_string(char *ptr);
void archivekit_free_buffer(uint8_t *ptr, size_t len);
```

Release ownership of returned strings and byte buffers. Passing NULL is safe.

## GZIP One-Shots

### `archivekit_gzip_compress`

```c
uint8_t* archivekit_gzip_compress(
    const uint8_t *input, size_t input_len, int level, size_t *out_len);
```

| Return | Meaning |
|---|---|
| pointer | GZIP bytes (free with `archivekit_free_buffer`) |
| NULL | Failure; check `archivekit_last_error` |

### `archivekit_gzip_decompress`

```c
uint8_t* archivekit_gzip_decompress(
    const uint8_t *input, size_t input_len, size_t *out_len);
```

Decompresses all members concatenated. Same return contract as compress.

### `archivekit_tar_gzip_compress`

```c
uint8_t* archivekit_tar_gzip_compress(
    const uint8_t *input, size_t input_len, int level, size_t *out_len);
```

Takes TAR archive bytes, returns `.tar.gz` bytes. Returns NULL (with an error set) when the input is not a readable TAR archive.

## Inspection

### `archivekit_detect_format`

```c
int archivekit_detect_format(const uint8_t *input, size_t input_len);
```

| Return | Meaning |
|---|---|
| 0 | Unknown |
| 1 | ZIP |
| 2 | GZIP |
| 3 | TAR |
| 4 | TAR+GZIP hint (reserved; currently reported as 2) |
| 5 | App (TAPP container) |

### `archivekit_list_names`

```c
char* archivekit_list_names(const uint8_t *input, size_t input_len);
```

| Return | Meaning |
|---|---|
| pointer | Newline-separated entry names (free with `archivekit_free_string`) |
| NULL | Unknown format or corrupt archive |

## Files and Directories

### `archivekit_compress_file`

```c
int archivekit_compress_file(const char *src, const char *dst, int format);
```

| Return | Meaning |
|---|---|
| 0 | Success |
| -1 | Bad arguments (null pointer, unknown format code) |
| -2 | Operation failed; check `archivekit_last_error` |

### `archivekit_extract`

```c
int archivekit_extract(const char *src, const char *dst_dir);
```

Same return contract. Format is auto-detected; `.tar.gz`/`.tgz` take the TAR+GZIP path.

### `archivekit_pack_dir`

```c
int archivekit_pack_dir(
    const char *src_dir, const char *dst, int format, int level);
```

| Return | Meaning |
|---|---|
| 0 | Success |
| -1 | Bad arguments |
| -2 | Pack or write failed |
| -3 | Parent directory of `dst` cannot be created |

## Usage / Example

```c
#include "archivekit.h"
#include <stdio.h>

int main(void) {
    const char *data = "hello tontoo";
    size_t len = 0;
    uint8_t *gz = archivekit_gzip_compress(
        (const uint8_t *)data, 12, 2, &len);
    if (!gz) {
        printf("error: %s\n", archivekit_last_error());
        return 1;
    }
    printf("format=%d bytes=%zu version=%s\n",
        archivekit_detect_format(gz, len), len, archivekit_version());
    archivekit_free_buffer(gz, len);

    if (archivekit_extract("/tmp/notes.tar.gz", "/tmp/restored") != 0) {
        printf("error: %s\n", archivekit_last_error());
        return 1;
    }
    return 0;
}
```

## App Containers

Opaque random-access `.app` readers. Only the footer plus the central directory are read on open; `archivekit_app_read` decodes a single entry.

### `archivekit_app_open` / `archivekit_app_close`

```c
CAppReader* archivekit_app_open(const char *path);
void archivekit_app_close(CAppReader *handle);
```

| Return (`open`) | Meaning |
|---|---|
| handle | Live reader |
| NULL | Failure; check `archivekit_last_error` |

`close` accepts NULL.

### `archivekit_app_list`

```c
char* archivekit_app_list(CAppReader *handle);
```

| Return | Meaning |
|---|---|
| pointer | Newline-separated entry names (free with `archivekit_free_string`) |
| NULL | Failure |

### `archivekit_app_read`

```c
uint8_t* archivekit_app_read(CAppReader *handle, const char *name, size_t *out_len);
```

| Return | Meaning |
|---|---|
| pointer | Entry payload (free with `archivekit_free_buffer`) |
| NULL | Missing entry or corrupt data |

### `archivekit_app_manifest`

```c
char* archivekit_app_manifest(CAppReader *handle, const char *field);
```

`field` is `bundle_id`, `version`, `executable`, `icon` or `name:<locale>`.

| Return | Meaning |
|---|---|
| pointer | Field value (free with `archivekit_free_string`) |
| NULL | Unknown field or unreadable manifest |

### `archivekit_app_extract` / `archivekit_app_pack`

```c
int archivekit_app_extract(const char *src, const char *dst_dir);
int archivekit_app_pack(const char *staging_dir, const char *dst, const char *app_name);
```

| Return | Meaning |
|---|---|
| 0 | Success |
| -1 | Bad arguments |
| -2 | Operation failed; check `archivekit_last_error` |
| -3 | (`pack` only) parent directory of `dst` cannot be created |

## ZIP Files (streaming, constant memory)

### `archivekit_zip_extract` / `archivekit_zip_list`

```c
int archivekit_zip_extract(const char *src, const char *dst_dir);
char* archivekit_zip_list(const char *src);
```

`archivekit_zip_extract` returns 0/-1/-2 like above. `archivekit_zip_list` returns newline-separated names (free with `archivekit_free_string`) reading only the tail plus the central directory, or NULL on error.

## Cross References

- [Combined.md](Combined.md) – Rust counterparts of the file helpers
- [App.md](App.md) – container layout, manifest schema, tico rules
- [Error.md](Error.md) – error meanings behind `archivekit_last_error`
