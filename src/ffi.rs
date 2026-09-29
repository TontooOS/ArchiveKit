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
        5 => Ok(Format::App),
        6 => Ok(Format::Tico),
        _ => {
            set_error(format!("unknown format code {format}"));
            Err(())
        }
    }
}

/// Library version string (do NOT free).
#[no_mangle]
pub extern "C" fn archivekit_version() -> *const c_char {
    const VERSION_C: &[u8] = b"27.0.0\0";
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

/// Detect the archive format: 0 = unknown, 1 = zip, 2 = gzip, 3 = tar,
/// 4 = tar.gz, 5 = app, 6 = tico.
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
        Some(Format::App) => 5,
        Some(Format::Tico) => 6,
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
) -> *mut c_char {    clear_error();
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

// ---------------------------------------------------------------------------
// .app containers (random access)
// ---------------------------------------------------------------------------

/// Opaque random-access `.app` reader (see `archivekit_app_open`).
pub struct CAppReader {
    inner: crate::AppReader<std::fs::File>,
}

/// Open a `.app` container for random access.
///
/// Only the footer plus the central directory are read; hundred-megabyte
/// apps open in milliseconds.
///
/// # Safety
/// `path` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn archivekit_app_open(path: *const c_char) -> *mut CAppReader {
    clear_error();
    let Ok(path) = cstr_to_path(path) else {
        return ptr::null_mut();
    };
    match crate::AppReader::open(&path) {
        Ok(inner) => Box::into_raw(Box::new(CAppReader { inner })),
        Err(e) => {
            set_error(e.to_string());
            ptr::null_mut()
        }
    }
}

/// Close a reader opened with `archivekit_app_open`.
///
/// # Safety
/// `handle` must come from `archivekit_app_open` (NULL is ignored).
#[no_mangle]
pub unsafe extern "C" fn archivekit_app_close(handle: *mut CAppReader) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle) });
    }
}

/// List entry names as newline-separated UTF-8 (free with
/// `archivekit_free_string`), or NULL on error.
///
/// # Safety
/// `handle` must be a live reader from `archivekit_app_open`.
#[no_mangle]
pub unsafe extern "C" fn archivekit_app_list(handle: *mut CAppReader) -> *mut c_char {
    clear_error();
    if handle.is_null() {
        set_error("null app handle".to_string());
        return ptr::null_mut();
    }
    let reader = unsafe { &mut *handle };
    match CString::new(reader.inner.list_names().join("\n")) {
        Ok(s) => s.into_raw(),
        Err(_) => {
            set_error("entry names contain NUL".to_string());
            ptr::null_mut()
        }
    }
}

/// Read one entry by full container path, e.g. `Foo.app/App/foo`.
///
/// Only this entry's bytes are read and decoded.
///
/// # Safety
/// `handle` must be live; `name` valid NUL-terminated; `out_len` writable.
#[no_mangle]
pub unsafe extern "C" fn archivekit_app_read(
    handle: *mut CAppReader,
    name: *const c_char,
    out_len: *mut usize,
) -> *mut u8 {
    clear_error();
    if handle.is_null() || name.is_null() {
        set_error("null pointer".to_string());
        return ptr::null_mut();
    }
    let reader = unsafe { &mut *handle };
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned();
    match reader.inner.read_file(&name) {
        Ok(out) => return_buffer(out, out_len),
        Err(e) => {
            set_error(e.to_string());
            ptr::null_mut()
        }
    }
}

/// Read one manifest field: `bundle_id`, `version`, `executable`, `icon`
/// or `name:<locale>` (e.g. `name:de_de`).
///
/// Returns an owned string (free with `archivekit_free_string`) or NULL.
///
/// # Safety
/// `handle` must be live; `field` valid NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn archivekit_app_manifest(
    handle: *mut CAppReader,
    field: *const c_char,
) -> *mut c_char {
    clear_error();
    if handle.is_null() || field.is_null() {
        set_error("null pointer".to_string());
        return ptr::null_mut();
    }
    let reader = unsafe { &mut *handle };
    let field = unsafe { CStr::from_ptr(field) }.to_string_lossy().into_owned();
    let value = match reader.inner.read_manifest() {
        Ok(m) => {
            if field == "bundle_id" {
                Some(m.bundle_id)
            } else if field == "version" {
                Some(m.version)
            } else if field == "executable" {
                Some(m.executable)
            } else if field == "icon" {
                m.icon
            } else if let Some(locale) = field.strip_prefix("name:") {
                m.name(locale).map(|s| s.to_string())
            } else {
                None
            }
        }
        Err(e) => {
            set_error(e.to_string());
            return ptr::null_mut();
        }
    };
    match value {
        Some(v) => match CString::new(v) {
            Ok(s) => s.into_raw(),
            Err(_) => {
                set_error("manifest value contains NUL".to_string());
                ptr::null_mut()
            }
        },
        None => {
            set_error(format!("unknown manifest field '{field}'"));
            ptr::null_mut()
        }
    }
}

/// Extract a `.app` container into a directory.
///
/// Returns 0 on success, negative on error.
///
/// # Safety
/// `src` and `dst_dir` must be valid NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn archivekit_app_extract(
    src: *const c_char,
    dst_dir: *const c_char,
) -> c_int {    clear_error();
    let (Ok(src), Ok(dst)) = (cstr_to_path(src), cstr_to_path(dst_dir)) else {
        return -1;
    };
    match crate::app::app_extract_to_file(&src, &dst) {
        Ok(()) => 0,
        Err(e) => {
            set_error(e.to_string());
            -2
        }
    }
}

/// Pack a staging tree (`App/`, `Resources/`, `Info.tontoo`) into a `.app`.
///
/// Returns 0 on success, negative on error.
///
/// # Safety
/// All three pointers must be valid NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn archivekit_app_pack(
    staging_dir: *const c_char,
    dst: *const c_char,
    app_name: *const c_char,
) -> c_int {
    clear_error();
    let (Ok(staging), Ok(dst), Ok(app_name)) = (
        cstr_to_path(staging_dir),
        cstr_to_path(dst),
        cstr_to_path(app_name),
    ) else {
        return -1;
    };
    let app_name = app_name.to_string_lossy().into_owned();
    match crate::app::app_pack_dir(&staging, &app_name, None) {
        Ok(bytes) => {
            if let Some(parent) = dst.parent() {
                if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
                    set_error("cannot create parent directory".to_string());
                    return -3;
                }
            }
            match std::fs::write(&dst, bytes) {
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

// ---------------------------------------------------------------------------
// ZIP files (streaming, constant memory)
// ---------------------------------------------------------------------------

/// Extract a `.zip` file into a directory with constant memory.
///
/// Returns 0 on success, negative on error.
///
/// # Safety
/// `src` and `dst_dir` must be valid NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn archivekit_zip_extract(
    src: *const c_char,
    dst_dir: *const c_char,
) -> c_int {
    clear_error();
    let (Ok(src), Ok(dst)) = (cstr_to_path(src), cstr_to_path(dst_dir)) else {
        return -1;
    };
    match crate::ZipFileReader::open(&src).and_then(|mut r| r.extract_all_to(&dst)) {
        Ok(()) => 0,
        Err(e) => {
            set_error(e.to_string());
            -2
        }
    }
}

/// List entry names of a `.zip` file as newline-separated UTF-8 (free with
/// `archivekit_free_string`), or NULL on error. Only the tail plus the
/// central directory are read.
///
/// # Safety
/// `src` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn archivekit_zip_list(src: *const c_char) -> *mut c_char {
    clear_error();
    let Ok(path) = cstr_to_path(src) else {
        return ptr::null_mut();
    };
    match crate::ZipFileReader::open(&path) {
        Ok(r) => {
            let joined = r
                .index()
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>()
                .join("\n");
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
