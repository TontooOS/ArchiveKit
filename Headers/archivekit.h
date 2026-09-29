/*
 * ArchiveKit - C Header
 * ZIP, GZIP, TAR and indexed .app / .tico containers for TontooOS
 * (codecs hand-written)
 *
 * Format codes: 1 = zip, 2 = gzip, 3 = tar, 4 = tar.gz, 5 = app, 6 = tico
 * Compression levels: 0 = none (stored), 1 = fastest, 2 = balanced, 3 = best
 */

#ifndef ARCHIVEKIT_H
#define ARCHIVEKIT_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ======================== */
/* Version / Errors         */
/* ======================== */

/**
 * Library version string (do NOT free).
 */
const char* archivekit_version(void);

/**
 * Last error message for this thread, or NULL when the last call succeeded.
 */
const char* archivekit_last_error(void);

/**
 * Free a string returned by ArchiveKit.
 *
 * @param ptr string to free (NULL is ignored)
 */
void archivekit_free_string(char *ptr);

/**
 * Free a byte buffer returned by ArchiveKit.
 *
 * @param ptr buffer to free (NULL is ignored)
 * @param len length from the matching call's out_len
 */
void archivekit_free_buffer(uint8_t *ptr, size_t len);

/* ======================== */
/* GZIP one-shots           */
/* ======================== */

/**
 * Compress bytes with GZIP.
 *
 * @param input input bytes
 * @param input_len input length
 * @param level 0..3 (none/fastest/balanced/best)
 * @param out_len output: compressed length
 * @return heap buffer (free with archivekit_free_buffer) or NULL on error
 */
uint8_t* archivekit_gzip_compress(const uint8_t *input, size_t input_len, int level, size_t *out_len);

/**
 * Decompress a GZIP stream (all members concatenated).
 *
 * @param input input bytes
 * @param input_len input length
 * @param out_len output: decompressed length
 * @return heap buffer (free with archivekit_free_buffer) or NULL on error
 */
uint8_t* archivekit_gzip_decompress(const uint8_t *input, size_t input_len, size_t *out_len);

/**
 * Compress TAR bytes with GZIP (.tar.gz).
 *
 * @param input TAR archive bytes
 * @param input_len input length
 * @param level 0..3
 * @param out_len output: compressed length
 * @return heap buffer (free with archivekit_free_buffer) or NULL on error
 */
uint8_t* archivekit_tar_gzip_compress(const uint8_t *input, size_t input_len, int level, size_t *out_len);

/* ======================== */
/* Inspection               */
/* ======================== */

/**
 * Detect the archive format.
 *
 * @param input input bytes
 * @param input_len input length
 * @return 0 = unknown, 1 = zip, 2 = gzip, 3 = tar, 4 = tar.gz, 5 = app,
 *         6 = tico
 */
int archivekit_detect_format(const uint8_t *input, size_t input_len);

/**
 * List entry names as newline-separated UTF-8.
 *
 * @param input archive bytes
 * @param input_len input length
 * @return string (free with archivekit_free_string) or NULL on error
 */
char* archivekit_list_names(const uint8_t *input, size_t input_len);

/* ======================== */
/* Files / Directories      */
/* ======================== */

/**
 * Compress a file or directory (format from code 1..6).
 *
 * @param src source file or directory
 * @param dst destination archive
 * @param format 1 = zip, 2 = gzip, 3 = tar, 4 = tar.gz, 5 = app (dir only),
 *         6 = tico (manifest + layers, use TicoBuilder instead)
 * @return 0 on success, negative on error (see archivekit_last_error)
 */
int archivekit_compress_file(const char *src, const char *dst, int format);

/**
 * Extract an archive (auto-detected, .tar.gz supported) into a directory.
 *
 * @param src archive file
 * @param dst_dir destination directory
 * @return 0 on success, negative on error
 */
int archivekit_extract(const char *src, const char *dst_dir);

/**
 * Pack a directory into an archive file.
 *
 * @param src_dir source directory
 * @param dst destination archive
 * @param format 1 = zip, 2 = gzip (rejected), 3 = tar, 4 = tar.gz, 5 = app,
 *         6 = tico
 * @param level 0..3
 * @return 0 on success, negative on error
 */
int archivekit_pack_dir(const char *src_dir, const char *dst, int format, int level);

/* ======================== */
/* ZIP files (streaming)    */
/* ======================== */

/**
 * Extract a .zip file into a directory with constant memory.
 *
 * @param src archive file
 * @param dst_dir destination directory
 * @return 0 on success, negative on error
 */
int archivekit_zip_extract(const char *src, const char *dst_dir);

/**
 * List entry names of a .zip file (tail + central directory only).
 *
 * @param src archive file
 * @return newline-separated names (free with archivekit_free_string) or NULL
 */
char* archivekit_zip_list(const char *src);

/* ======================== */
/* .app containers          */
/* ======================== */

/** Opaque random-access .app reader. */
typedef struct CAppReader CAppReader;

/**
 * Open a .app container (reads footer + central directory only).
 *
 * @param path container file
 * @return handle or NULL on error (see archivekit_last_error)
 */
CAppReader* archivekit_app_open(const char *path);

/**
 * Close a reader.
 *
 * @param handle handle or NULL (ignored)
 */
void archivekit_app_close(CAppReader *handle);

/**
 * List entry names as newline-separated UTF-8.
 *
 * @param handle live reader
 * @return string (free with archivekit_free_string) or NULL on error
 */
char* archivekit_app_list(CAppReader *handle);

/**
 * Read one entry (only its bytes are decoded).
 *
 * @param handle live reader
 * @param name full container path, e.g. "Foo.app/App/foo"
 * @param out_len output: payload length
 * @return heap buffer (free with archivekit_free_buffer) or NULL on error
 */
uint8_t* archivekit_app_read(CAppReader *handle, const char *name, size_t *out_len);

/**
 * Read a manifest field: "bundle_id", "version", "executable", "icon"
 * or "name:<locale>".
 *
 * @param handle live reader
 * @param field field name
 * @return string (free with archivekit_free_string) or NULL on error
 */
char* archivekit_app_manifest(CAppReader *handle, const char *field);

/**
 * Extract a .app container into a directory.
 *
 * @param src container file
 * @param dst_dir destination directory
 * @return 0 on success, negative on error
 */
int archivekit_app_extract(const char *src, const char *dst_dir);

/**
 * Pack a staging tree (App/, Resources/, Info.tontoo) into a .app.
 *
 * @param staging_dir staging directory
 * @param dst destination .app file
 * @param app_name top prefix stem, e.g. "Foo" for "Foo.app/"
 * @return 0 on success, negative on error
 */
int archivekit_app_pack(const char *staging_dir, const char *dst, const char *app_name);

#ifdef __cplusplus
}
#endif

#endif /* ARCHIVEKIT_H */
