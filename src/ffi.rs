//! C FFI for ArchiveKit (see `Headers/archivekit.h`).
//!
//! All functions are `no_mangle extern "C"`. Buffers returned to C are
//! heap-allocated; release them with [`archivekit_free_buffer`] (bytes) or
//! [`archivekit_free_string`] (strings). [`archivekit_last_error`] exposes
//! a thread-local message for the most recent failure.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, CStr, CString};
use std::ptr;
use std::slice;

use crate::combined::Format;
use crate::deflate::CompressionLevel;

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_error(msg: String) {
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = CString::new(msg).ok();
    });
}

fn cstr_to_path<'a>(ptr: *const c_char) -> Result<std::path::PathBuf, ()> {
    if ptr.is_null() {
        set_error("null path pointer".to_string());
        return Err(());
    }
    let s = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
    Ok(std::path::PathBuf::from(s))
}

fn level_from_int(level: c_int) -> CompressionLevel {
    match level {
        0 => CompressionLevel::None,
        1 => CompressionLevel::Fastest,
        3 => CompressionLevel::Best,
        _ => CompressionLevel::Balanced,
    }
}

fn format_from_int(format: c_int) -> Result<Format, ()> {
    match format {
        1 => Ok(Format::Zip),
        2 => Ok(Format::Gzip),
        3 => Ok(Format::Tar),
        4 => Ok(Format::TarGzip),
        _ => {
            set_error(format!("unknown format code {format}"));
            Err(())
        }
    }
}

/// Library version string (do NOT free).
#[no_mangle]
pub extern "C" fn archivekit_version() -> *const c_char {
    const VERSION_C: &[u8] = b"26.1.0\0";
    VERSION_C.as_ptr() as *const c_char
}

/// Last error message for this thread, or NULL when the last call succeeded.
#[no_mangle]
pub extern "C" fn archivekit_last_error() -> *const c_char {
    LAST_ERROR.with(|e| match e.borrow().as_ref() {
        Some(s) => s.as_ptr(),
        None => ptr::null(),
    })
}

fn clear_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

/// Free a string returned by ArchiveKit (`archivekit_list_names_json`, ...).
///
/// # Safety
/// `ptr` must come from an ArchiveKit string function.
#[no_mangle]
pub unsafe extern "C" fn archivekit_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

/// Free a byte buffer returned by ArchiveKit.
///
/// # Safety
/// `ptr`/`len` must come from the matching ArchiveKit buffer function.
#[no_mangle]
pub unsafe extern "C" fn archivekit_free_buffer(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(slice::from_raw_parts_mut(ptr, len)) });
    }
}

fn return_buffer(mut data: Vec<u8>, out_len: *mut usize) -> *mut u8 {
    if out_len.is_null() {
        set_error("null out_len pointer".to_string());
        return ptr::null_mut();
    }
    unsafe {
        *out_len = data.len();
    }
    let boxed: Box<[u8]> = data.drain(..).collect();
    Box::into_raw(boxed) as *mut u8
}

/// Compress bytes with GZIP.
///
/// # Safety
/// `input` must point to `input_len` readable bytes; `out_len` must be writable.
#[no_mangle]
pub unsafe extern "C" fn archivekit_gzip_compress(
    input: *const u8,
    input_len: usize,
    level: c_int,
    out_len: *mut usize,
) -> *mut u8 {
    clear_error();
    if input.is_null() && input_len != 0 {
        set_error("null input pointer".to_string());
        return ptr::null_mut();
    }
    let bytes = if input_len == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(input, input_len) }
    };
    let out = crate::gzip_compress(bytes, level_from_int(level));
    return_buffer(out, out_len)
}

/// Decompress a GZIP stream (all members concatenated).
///
/// # Safety
/// Same pointer contract as `archivekit_gzip_compress`.
#[no_mangle]
pub unsafe extern "C" fn archivekit_gzip_decompress(
    input: *const u8,
    input_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    clear_error();
    if input.is_null() && input_len != 0 {
        set_error("null input pointer".to_string());
        return ptr::null_mut();
    }
    let bytes = if input_len == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(input, input_len) }
    };
    match crate::gzip_decompress(bytes) {
        Ok(out) => return_buffer(out, out_len),
        Err(e) => {
            set_error(e.to_string());
            ptr::null_mut()
        }
    }
}

/// Compress TAR bytes with GZIP (`.tar.gz`).
///
/// # Safety
/// Same pointer contract as `archivekit_gzip_compress`.
#[no_mangle]
pub unsafe extern "C" fn archivekit_tar_gzip_compress(
    input: *const u8,
    input_len: usize,
    level: c_int,
    out_len: *mut usize,
) -> *mut u8 {
    clear_error();
    if input.is_null() && input_len != 0 {
        set_error("null input pointer".to_string());
        return ptr::null_mut();
    }
    let bytes = if input_len == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(input, input_len) }
    };
    match crate::tar_unpack(bytes) {
        Ok(entries) => {
            let out = crate::tar_gzip_compress(&entries, level_from_int(level));
            return_buffer(out, out_len)
        }
        Err(e) => {
            set_error(e.to_string());
            ptr::null_mut()
        }
    }
}

/// Detect the archive format: 0 = unknown, 1 = zip, 2 = gzip, 3 = tar.
///
/// # Safety
/// `input` must point to `input_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn archivekit_detect_format(input: *const u8, input_len: usize) -> c_int {
    clear_error();
    if input.is_null() || input_len == 0 {
        return 0;
    }
    let bytes = unsafe { slice::from_raw_parts(input, input_len) };
    match crate::detect_format(bytes) {
        Some(Format::Zip) => 1,
        Some(Format::Gzip) => 2,
        Some(Format::Tar) => 3,
        Some(Format::TarGzip) => 4,
        None => 0,
    }
}

/// Compress a file or directory; format from `format` (1..4).
///
/// Returns 0 on success, negative on error (see `archivekit_last_error`).
///
/// # Safety
/// `src` and `dst` must be valid NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn archivekit_compress_file(
    src: *const c_char,
    dst: *const c_char,
    format: c_int,
) -> c_int {
    clear_error();
    let (Ok(src), Ok(dst), Ok(format)) = (
        cstr_to_path(src),
        cstr_to_path(dst),
        format_from_int(format),
    ) else {
        return -1;
    };
    match crate::compress_file(&src, &dst, Some(format)) {
        Ok(()) => 0,
        Err(e) => {
            set_error(e.to_string());
            -2
        }
    }
}

/// Extract an archive (auto-detected, `.tar.gz` supported) into a directory.
///
/// Returns 0 on success, negative on error.
///
/// # Safety
/// `src` and `dst_dir` must be valid NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn archivekit_extract(
    src: *const c_char,
    dst_dir: *const c_char,
) -> c_int {
    clear_error();
    let (Ok(src), Ok(dst)) = (cstr_to_path(src), cstr_to_path(dst_dir)) else {
        return -1;
    };
    match crate::extract_archive(&src, &dst) {
        Ok(()) => 0,
        Err(e) => {
            set_error(e.to_string());
            -2
        }
    }
}

/// Pack a directory into an archive file; format from `format` (1..4).
///
/// Returns 0 on success, negative on error.
///
/// # Safety
/// `src_dir` and `dst` must be valid NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn archivekit_pack_dir(
    src_dir: *const c_char,
    dst: *const c_char,
    format: c_int,
    level: c_int,
) -> c_int {
    clear_error();
    let (Ok(src), Ok(dst), Ok(format)) = (
        cstr_to_path(src_dir),
        cstr_to_path(dst),
        format_from_int(format),
    ) else {
        return -1;
    };
    let level = level_from_int(level);
    let bytes = crate::pack_dir_to_archive(&src, format, level);
    match bytes {
        Ok(b) => {
            if let Some(parent) = dst.parent() {
                if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
                    set_error("cannot create parent directory".to_string());
                    return -3;
                }
            }
            match std::fs::write(&dst, b) {
                Ok(()) => 0,
                Err(e) => {
                    set_error(e.to_string());
                    -2
                }
            }
        }
        Err(e) => {
            set_error(e.to_string());
            -2
        }
    }
}

/// List entry names as newline-separated UTF-8 (free with
/// `archivekit_free_string`), or NULL on error.
///
/// # Safety
/// `input` must point to `input_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn archivekit_list_names(
    input: *const u8,
    input_len: usize,
) -> *mut c_char {
    clear_error();
    if input.is_null() || input_len == 0 {
        set_error("null or empty input".to_string());
        return ptr::null_mut();
    }
    let bytes = unsafe { slice::from_raw_parts(input, input_len) };
    match crate::list_names(bytes) {
        Ok(names) => {
            let joined = names.join("\n");
            match CString::new(joined) {
                Ok(s) => s.into_raw(),
                Err(_) => {
                    set_error("entry names contain NUL".to_string());
                    ptr::null_mut()
                }
            }
        }
        Err(e) => {
            set_error(e.to_string());
            ptr::null_mut()
        }
    }
}
