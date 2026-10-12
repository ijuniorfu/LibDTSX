//! C API of the LibDTSX decoder. See `include/dtsx.h` for the contract.
//!
//! Every exported function catches panics: none unwinds into C. A panic inside the decode of
//! one frame is recovered in `engine` like any other decode error; one caught here, at the
//! boundary, poisons the handle until `dtsx_decoder_reset`.

mod engine;
mod framer;
pub mod position;
pub mod types;

use engine::Engine;
use std::ffi::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use types::*;

/// Opaque handle type of the C API.
pub struct DTSXDecoder {
    _private: [u8; 0],
}

const VERSION: &std::ffi::CStr = c"dtsx-ffi 0.1.0 (dca 0.8.0, harletty-bridge@a717bae441b1)";

fn engine_mut<'a>(handle: *mut DTSXDecoder) -> Option<&'a mut Engine> {
    // SAFETY: the C contract is that a non-null handle came from create() and is used by one
    // thread at a time until destroy().
    unsafe { (handle as *mut Engine).as_mut() }
}

fn engine_ref<'a>(handle: *const DTSXDecoder) -> Option<&'a Engine> {
    // SAFETY: as above.
    unsafe { (handle as *const Engine).as_ref() }
}

/// Runs `f` on the engine, turning a panic into ERR_PANIC and poisoning the handle.
fn with_engine(
    handle: *mut DTSXDecoder,
    allow_poisoned: bool,
    f: impl FnOnce(&mut Engine) -> Status,
) -> Status {
    let Some(engine) = engine_mut(handle) else {
        return ERR_NULL;
    };
    if engine.poisoned && !allow_poisoned {
        return ERR_PANIC;
    }
    match catch_unwind(AssertUnwindSafe(|| f(engine))) {
        Ok(status) => status,
        Err(_) => {
            // The closure's borrow ended when it unwound; take a fresh one from the handle.
            if let Some(engine) = engine_mut(handle) {
                engine.poisoned = true;
            }
            ERR_PANIC
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn dtsx_api_version() -> u32 {
    API_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn dtsx_version_string() -> *const c_char {
    VERSION.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn dtsx_speaker_name(speaker: u8) -> *const c_char {
    position::speaker_name(speaker).as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn dtsx_presentation_name(presentation: u32) -> *const c_char {
    engine::presentation_name(presentation).as_ptr()
}

/// Diagnostic: raises a Rust panic inside the library and catches it, the way every exported
/// function would. Returns OK when unwinding works in the final link (the panic handler prints
/// one line to stderr).
#[unsafe(no_mangle)]
pub extern "C" fn dtsx_selftest() -> Status {
    let caught = catch_unwind(|| {
        let depth = std::hint::black_box(3usize);
        if depth > 2 {
            panic!("dtsx_selftest: deliberate panic, caught");
        }
        depth
    });
    if caught.is_err() { OK } else { ERR_PANIC }
}

#[unsafe(no_mangle)]
pub extern "C" fn dtsx_decoder_create() -> *mut DTSXDecoder {
    catch_unwind(|| Box::into_raw(Engine::new()) as *mut DTSXDecoder)
        .unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// `decoder` is NULL or a handle from `dtsx_decoder_create` not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dtsx_decoder_destroy(decoder: *mut DTSXDecoder) {
    if decoder.is_null() {
        return;
    }
    // SAFETY: per the contract, the pointer came from Box::into_raw in create().
    let engine = unsafe { Box::from_raw(decoder as *mut Engine) };
    // A panic in a destructor must not unwind into C either.
    let _ = catch_unwind(AssertUnwindSafe(move || drop(engine)));
}

/// # Safety
/// `decoder` is NULL or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dtsx_decoder_reset(decoder: *mut DTSXDecoder) -> Status {
    let status = with_engine(decoder, true, |engine| {
        engine.reset();
        OK
    });
    if status == ERR_PANIC {
        // Rebuild from scratch; a failed reset must not leave the handle unusable.
        if let Some(engine) = engine_mut(decoder)
            && let Ok(fresh) = catch_unwind(Engine::new)
        {
            *engine = *fresh;
            return OK;
        }
    }
    status
}

/// # Safety
/// `decoder` is NULL or a live handle; `data` points to `size` readable bytes (or `size` is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dtsx_decoder_push(
    decoder: *mut DTSXDecoder,
    data: *const u8,
    size: usize,
    pts: i64,
) -> Status {
    if size == 0 {
        return if decoder.is_null() { ERR_NULL } else { OK };
    }
    if data.is_null() {
        return ERR_NULL;
    }
    // SAFETY: the caller guarantees `size` readable bytes at `data`.
    let bytes = unsafe { std::slice::from_raw_parts(data, size) };
    with_engine(decoder, false, |engine| engine.push(bytes, pts))
}

/// # Safety
/// `decoder` is NULL or a live handle; `block` is NULL or points to a writable DTSXBlock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dtsx_decoder_pull(decoder: *mut DTSXDecoder, block: *mut Block) -> Status {
    // SAFETY: the caller guarantees `block` is NULL or valid for writes.
    let Some(out) = (unsafe { block.as_mut() }) else {
        return ERR_NULL;
    };
    with_engine(decoder, false, |engine| engine.pull(out))
}

/// # Safety
/// `decoder` is NULL or a live handle; `info` is NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dtsx_decoder_get_stream_info(
    decoder: *const DTSXDecoder,
    info: *mut StreamInfo,
) -> Status {
    let (Some(engine), Some(out)) = (engine_ref(decoder), unsafe { info.as_mut() }) else {
        return ERR_NULL;
    };
    match catch_unwind(AssertUnwindSafe(|| engine.stream_info())) {
        Ok(Some(stream_info)) => {
            *out = stream_info;
            OK
        }
        Ok(None) => ERR_NOT_READY,
        Err(_) => ERR_PANIC,
    }
}

/// # Safety
/// `decoder` is NULL or a live handle; `stats` is NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dtsx_decoder_get_stats(
    decoder: *const DTSXDecoder,
    stats: *mut Stats,
) -> Status {
    let (Some(engine), Some(out)) = (engine_ref(decoder), unsafe { stats.as_mut() }) else {
        return ERR_NULL;
    };
    match catch_unwind(AssertUnwindSafe(|| engine.stats())) {
        Ok(s) => {
            *out = s;
            OK
        }
        Err(_) => ERR_PANIC,
    }
}

/// # Safety
/// `decoder` is NULL or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dtsx_decoder_last_error(decoder: *const DTSXDecoder) -> *const c_char {
    match engine_ref(decoder) {
        Some(engine) => engine.last_error().as_ptr(),
        None => c"".as_ptr(),
    }
}
