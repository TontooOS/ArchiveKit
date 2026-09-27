/*
 * ArchiveKit - C Header
 * ZIP, GZIP and TAR compression for TontooOS (100% hand-written, no dependencies)
 *
 * Format codes: 1 = zip, 2 = gzip, 3 = tar, 4 = tar.gz
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
 * @return 0 = unknown, 1 = zip, 2 = gzip, 3 = tar, 4 = tar.gz
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
 * Compress a file or directory (format from code 1..4).
 *
 * @param src source file or directory
 * @param dst destination archive
 * @param format 1 = zip, 2 = gzip, 3 = tar, 4 = tar.gz
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
 * @param format 1 = zip, 2 = gzip (rejected), 3 = tar, 4 = tar.gz
 * @param level 0..3
 * @return 0 on success, negative on error
 */
int archivekit_pack_dir(const char *src_dir, const char *dst, int format, int level);

#ifdef __cplusplus
}
#endif

#endif /* ARCHIVEKIT_H */
