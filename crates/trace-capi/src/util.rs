//! FFI plumbing: panic guard, error-string channel, result arena, and the
//! string-free entry point.

use crate::types::TraceStatus;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Typed domain error for the C boundary. `status()` picks the documented
/// status code at the point the error is created, so a new call site cannot
/// forget to classify the errors it produces.
#[derive(Debug)]
pub(crate) enum ApiError {
    /// Bad caller input / ABI mismatch → `TraceErrInvalidArg`.
    InvalidArg(String),
    /// Filesystem error → `TraceErrIo`.
    Io(String),
    /// Queried entity does not exist → `TraceErrNotFound`.
    NotFound(String),
    /// Pipeline / query failure → `TraceErrAnalysis`.
    Analysis(String),
    /// Rust panic caught at the boundary → `TraceErrPanic`.
    Panic(String),
}

impl ApiError {
    pub(crate) fn message(&self) -> &str {
        match self {
            ApiError::InvalidArg(m)
            | ApiError::Io(m)
            | ApiError::NotFound(m)
            | ApiError::Analysis(m)
            | ApiError::Panic(m) => m,
        }
    }

    pub(crate) fn status(&self) -> i32 {
        match self {
            ApiError::InvalidArg(_) => TraceStatus::TraceErrInvalidArg as i32,
            ApiError::Io(_) => TraceStatus::TraceErrIo as i32,
            ApiError::NotFound(_) => TraceStatus::TraceErrNotFound as i32,
            ApiError::Analysis(_) => TraceStatus::TraceErrAnalysis as i32,
            ApiError::Panic(_) => TraceStatus::TraceErrPanic as i32,
        }
    }
}

/// Wrap an error produced by the `trace-db` export/inspect layer. The
/// classification reads the *whole* error chain, not just the display text:
/// a filesystem failure — e.g. `export_to_sqlite` hitting an unwritable
/// output or a directory standing where a file must be written — surfaces as
/// `TRACE_ERR_IO` rather than `TRACE_ERR_ANALYSIS`. A `not found`-family
/// message means the queried entity is absent (`NotFound`); everything else
/// is an `Analysis` failure.
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        if e.downcast_ref::<std::io::Error>().is_some() {
            return ApiError::Io(format!("{e:#}"));
        }
        if let Some(sqlite) = e.downcast_ref::<rusqlite::Error>() {
            if is_sqlite_io_error(sqlite) {
                return ApiError::Io(format!("{e:#}"));
            }
        }
        let msg = format!("{e:#}");
        if msg.contains("not found") || msg.contains("no value-flow node") {
            ApiError::NotFound(msg)
        } else {
            ApiError::Analysis(msg)
        }
    }
}

/// True when a `rusqlite::Error` is a filesystem-level failure (unable to
/// open/create the database file, read-only output, disk I/O error, full
/// disk) rather than a SQL/schema/constraint error, which stays an
/// `Analysis` failure.
fn is_sqlite_io_error(e: &rusqlite::Error) -> bool {
    use rusqlite::ErrorCode;
    match e {
        rusqlite::Error::InvalidPath(_) => true,
        rusqlite::Error::SqliteFailure(err, _) => matches!(
            err.code,
            ErrorCode::CannotOpen
                | ErrorCode::ReadOnly
                | ErrorCode::SystemIoFailure
                | ErrorCode::DiskFull
                | ErrorCode::PermissionDenied
                | ErrorCode::NoLargeFileSupport
        ),
        _ => false,
    }
}

/// Run `body`, catching Rust panics so none cross the C boundary. Domain
/// errors come back as `ApiError`s (classified at their creation site);
/// panics become `ApiError::Panic`.
pub(crate) fn guard<T>(body: impl FnOnce() -> Result<T, ApiError>) -> Result<T, ApiError> {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(err)) => Err(err),
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic payload".to_string());
            Err(ApiError::Panic(format!("internal panic: {detail}")))
        }
    }
}

/// Write `msg` into `*out_err` (C-owned; freed with `trace_string_free`).
/// No-op when `out_err` is null.
pub(crate) unsafe fn set_error(out_err: *mut *mut c_char, msg: &str) {
    if out_err.is_null() {
        return;
    }
    if let Ok(cs) = CString::new(msg) {
        *out_err = cs.into_raw();
    }
}

/// Null `*out_err` at the start of a call so a stale pointer from a previous
/// call can never be observed or double-freed by a caller that only checks
/// `*out_err != NULL`. No-op when `out_err` is null.
pub(crate) unsafe fn reset_err(out_err: *mut *mut c_char) {
    if !out_err.is_null() {
        *out_err = std::ptr::null_mut();
    }
}

/// Read a NUL-terminated C string. The returned reference is valid for the
/// caller's input buffer; callers copy it into owned data before returning.
pub(crate) unsafe fn cstr<'a>(s: *const c_char) -> Result<&'a str, ApiError> {
    if s.is_null() {
        return Err(ApiError::InvalidArg("null string argument".to_string()));
    }
    CStr::from_ptr(s)
        .to_str()
        .map_err(|e| ApiError::InvalidArg(format!("argument is not valid UTF-8: {e}")))
}

/// Read a C array of C strings (`argv`-style) into owned `String`s.
pub(crate) unsafe fn str_array(
    arr: *const *const c_char,
    n: usize,
) -> Result<Vec<String>, ApiError> {
    if n == 0 {
        return Ok(Vec::new());
    }
    if arr.is_null() {
        return Err(ApiError::InvalidArg(
            "string array is null with length > 0".to_string(),
        ));
    }
    let slice = std::slice::from_raw_parts(arr, n);
    let mut out = Vec::with_capacity(n);
    for &p in slice {
        out.push(cstr(p)?.to_owned());
    }
    Ok(out)
}

/// Append-only store of NUL-terminated C strings. Each `CString` is heap
/// allocated once and its buffer is never moved, so pointers handed out by
/// `add` stay valid until the arena is dropped. Dropping the arena frees
/// every string it holds.
#[derive(Default)]
pub(crate) struct Arena {
    strings: Vec<CString>,
}

impl Arena {
    pub fn new() -> Self {
        Self::default()
    }

    /// Copy `s` into the arena and return a stable `char*`.
    pub fn add(&mut self, s: &str) -> *const c_char {
        let cs = CString::new(s).unwrap_or_else(|_| CString::new("<invalid>").unwrap());
        let ptr = cs.as_ptr();
        self.strings.push(cs);
        ptr
    }

    /// Copy an optional string; `None` maps to a null pointer.
    pub fn add_opt(&mut self, s: Option<&str>) -> *const c_char {
        match s {
            Some(v) => self.add(v),
            None => std::ptr::null(),
        }
    }
}

/// Free a string previously returned by this library (error messages).
/// Safe on null; passing any other pointer is undefined behavior.
///
/// # Safety
///
/// The pointer must come from `into_raw` of a `CString` produced here.
#[no_mangle]
pub unsafe extern "C" fn trace_string_free(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    drop(CString::from_raw(s));
}
