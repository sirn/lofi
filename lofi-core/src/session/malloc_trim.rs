//! Best-effort release of freed heap pages back to the OS.
//!
//! The IO worker thread accumulates freed pages in its glibc arena as tree
//! snapshots and hydration HashMaps come and go. Glibc's default trim
//! threshold only fires when the arena top is mostly free; the worker's
//! steady-state working set sits below that, so RSS ratchets up by the
//! peak /tree working set the first time the picker opens.
//!
//! `release_freed_memory` calls `malloc_trim(0)` to force-release all free
//! pages at the top of every arena. We invoke it from the IO worker after a
//! tree snapshot is dropped, so the pages freed on this thread return to the
//! kernel promptly.

/// Release freed heap pages back to the OS when supported.
///
/// On glibc this calls `malloc_trim(0)`. Other allocators and platforms
/// either don't expose an equivalent or handle decay internally; in both
/// cases this is a no-op.
pub fn release_freed_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[allow(unsafe_code)]
    unsafe {
        // SAFETY: malloc_trim is thread-safe per glibc docs and has no
        // user-visible side effects beyond returning freed pages to the OS.
        // The argument 0 asks it to release all available pages, not just
        // some pad amount.
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        let _ = malloc_trim(0);
    }
}
