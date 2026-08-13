//! Best-effort release of freed heap pages back to the OS.
//!
//! Glibc's default trim threshold only fires when an arena top is mostly
//! free. Agent turns, compaction, and /tree snapshots allocate a burst
//! and then drop it; the steady-state working set sits below that
//! threshold, so RSS ratchets by the peak unless we force a trim.
//!
//! `release_freed_memory` calls `malloc_trim(0)` to release free pages
//! from every arena. Call it on the thread that just dropped the burst.

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

/// Trim when this value is dropped. Hold it across a function that
/// allocates a burst and then frees it, so every return path trims.
pub struct ReleaseFreedMemoryOnDrop;

impl Drop for ReleaseFreedMemoryOnDrop {
    fn drop(&mut self) {
        release_freed_memory();
    }
}
