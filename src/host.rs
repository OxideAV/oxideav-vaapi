//! Pluggable symbol resolution and native-call bracketing.
//!
//! By default the bridge `dlopen`s `libva.so.2` / `libva-drm.so.2`
//! itself (the `system-loader` feature, through `libloading`). A host
//! that reaches libva some other way installs a [`HostLoader`] before
//! the first VA call instead:
//!
//! * `resolve` returns the address of a named function, or null. It is
//!   asked for every VA entry point the bridge uses and for the C
//!   library's `open` and `close` (the render node is opened with the
//!   same C library that libva uses). The host decides which libraries
//!   those come from and keeps them loaded for the process lifetime.
//! * `enter` / `exit`, when present, bracket every call into a resolved
//!   function: `exit` receives the value `enter` returned. A host whose
//!   native code needs per-thread state switched (a different C
//!   library's thread pointer, a signal mask, ...) does it there. The
//!   bridge's own code never runs between `enter` and `exit`.
//!
//! The render node is opened with Linux's `O_RDWR | O_CLOEXEC` values
//! when a host loader is installed, since the resolved `open` is the
//! Linux C library's.

use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::OnceLock;

/// Returns the address of the function `name` (NUL-terminated), or null.
pub type ResolveFn = unsafe extern "C" fn(context: *mut c_void, name: *const c_char) -> *mut c_void;
/// Called before each native call; the value is passed to the matching [`ExitFn`].
pub type EnterFn = unsafe extern "C" fn() -> usize;
/// Called after each native call with the value [`EnterFn`] returned.
pub type ExitFn = unsafe extern "C" fn(usize);

/// A host-provided way to reach libva (see the module documentation).
#[derive(Clone, Copy, Debug)]
pub struct HostLoader {
    /// Passed unchanged to `resolve`.
    pub context: *mut c_void,
    pub resolve: ResolveFn,
    pub enter: Option<EnterFn>,
    pub exit: Option<ExitFn>,
}

// SAFETY: the loader is a set of function pointers and an opaque
// context the host promises is usable from any thread.
unsafe impl Send for HostLoader {}
unsafe impl Sync for HostLoader {}

static HOST: OnceLock<HostLoader> = OnceLock::new();

/// Install the host loader. Must happen before anything else in this
/// crate touches libva; the first installation wins and later calls
/// return `false`. Returns `false` too when the bridge has already
/// loaded libva some other way.
///
/// # Safety
/// `resolve` must return functions with the C prototypes libva
/// documents (and the C library's `open` / `close`), valid for the
/// process lifetime; `enter` / `exit` must be callable from any thread.
pub unsafe fn install(loader: HostLoader) -> bool {
    if crate::sys::vtable_loaded() {
        return false;
    }
    HOST.set(loader).is_ok()
}

pub(crate) fn installed() -> Option<&'static HostLoader> {
    HOST.get()
}

/// Run one native call, bracketed by the host's `enter` / `exit`.
#[inline]
pub(crate) fn native<R>(call: impl FnOnce() -> R) -> R {
    match HOST.get() {
        Some(HostLoader {
            enter: Some(enter),
            exit: Some(exit),
            ..
        }) => {
            // SAFETY: the host promised both are callable from any thread.
            let token = unsafe { enter() };
            let result = call();
            unsafe { exit(token) };
            result
        }
        _ => call(),
    }
}
