//! Return freed heap pages at phase boundaries and indexing checkpoints on glibc.

pub(crate) fn reclaim_unused_pages() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        // glibc synchronizes access to all arenas. Trims can stall allocating
        // workers, so callers use phase boundaries or bounded, infrequent
        // checkpoints after completed indexing payloads have been consumed.
        unsafe { malloc_trim(0) };
    }
}
