//! Best-effort release of freed heap pages back to the OS.
//!
//! Glibc retains freed pages in allocator arenas after burst allocations.
//! Call this at the boundary that owns and drops a transient working set.

/// Release freed heap pages back to the OS when supported.
///
/// On glibc this calls `malloc_trim(0)`. Other allocators and platforms
/// either do not expose an equivalent or handle decay internally.
pub fn release_freed_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[allow(unsafe_code)]
    unsafe {
        // SAFETY: malloc_trim is thread-safe per glibc documentation and has
        // no visible effect except returning unused allocator pages to the OS.
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        let _ = malloc_trim(0);
    }
}

/// Release freed memory when a transient allocation scope exits.
#[must_use = "dropping immediately trims before the transient scope exits"]
pub struct ReleaseFreedMemoryOnDrop;

impl Drop for ReleaseFreedMemoryOnDrop {
    fn drop(&mut self) {
        release_freed_memory();
    }
}
