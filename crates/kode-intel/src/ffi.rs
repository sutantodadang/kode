//! Hand-written bindings for the zindeks C ABI (version 1).
//!
//! These mirror `include/zindeks.h` from the zindeks repository exactly. No
//! bindgen or build-time compiler is used: the structs are `repr(C)` and the
//! function pointers are looked up at runtime through `libloading`, so Kode
//! never needs a Zig toolchain or a build-time link against the engine.

use libloading::Library;

/// ABI version this crate was written against. A loaded library that reports a
/// different value is refused before any handle is created.
pub const ABI_VERSION: u32 = 1;

pub const STATUS_OK: i32 = 0;
pub const STATUS_INVALID_INPUT: i32 = 1;
pub const STATUS_INIT_ERROR: i32 = 2;
pub const STATUS_ALLOC_ERROR: i32 = 3;
pub const STATUS_BUSY: i32 = 4;

/// Maximum accepted request body (1 MiB), mirroring `ZINDEKS_MAX_REQUEST_BYTES`.
pub const MAX_REQUEST_BYTES: usize = 1 << 20;
/// Maximum produced response body (16 MiB), mirroring `ZINDEKS_MAX_RESPONSE_BYTES`.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Library-owned byte buffer (`ZindeksBuffer`).
#[repr(C)]
#[derive(Debug)]
pub struct ZindeksBuffer {
    pub ptr: *mut u8,
    pub len: usize,
}

impl ZindeksBuffer {
    pub const fn empty() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            len: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ptr.is_null() || self.len == 0
    }
}

/// Opaque handle bound to one repository (`ZindeksHandle`). Zero-sized because
/// the layout is owned by the library and never inspected by the host.
#[repr(C)]
pub struct ZindeksHandle {
    _private: [u8; 0],
}

pub type AbiVersionFn = unsafe extern "C" fn() -> u32;
pub type OpenFn = unsafe extern "C" fn(
    options: *const u8,
    len: usize,
    out: *mut *mut ZindeksHandle,
    error: *mut ZindeksBuffer,
) -> i32;
pub type RequestFn = unsafe extern "C" fn(
    handle: *mut ZindeksHandle,
    json: *const u8,
    len: usize,
    response: *mut ZindeksBuffer,
) -> i32;
pub type BufferFreeFn = unsafe extern "C" fn(buffer: *mut ZindeksBuffer);
pub type CloseFn = unsafe extern "C" fn(handle: *mut ZindeksHandle);

/// The five ABI 1 entry points, resolved once when the library is loaded.
pub struct Symbols {
    pub abi_version: AbiVersionFn,
    pub open: OpenFn,
    pub request: RequestFn,
    pub buffer_free: BufferFreeFn,
    pub close: CloseFn,
}

impl Symbols {
    /// Resolves every ABI 1 symbol from an already-loaded library.
    ///
    /// # Safety
    /// `library` must stay loaded for at least as long as the returned
    /// `Symbols` is used. The caller owns that invariant by keeping the
    /// `Library` and the `Symbols` in the same owner.
    pub unsafe fn load(library: &Library) -> Result<Self, libloading::Error> {
        // SAFETY: the caller upholds that `library` outlives the returned
        // function pointers; symbol names are the ABI 1 contract.
        unsafe {
            Ok(Self {
                abi_version: *library.get(b"zindeks_abi_version\0")?,
                open: *library.get(b"zindeks_open\0")?,
                request: *library.get(b"zindeks_request\0")?,
                buffer_free: *library.get(b"zindeks_buffer_free\0")?,
                close: *library.get(b"zindeks_close\0")?,
            })
        }
    }
}
