//! Return freed heap pages at phase boundaries and indexing checkpoints on glibc and Windows.

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
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
        }
        #[link(name = "psapi")]
        unsafe extern "system" {
            fn EmptyWorkingSet(h_process: *mut std::ffi::c_void) -> std::ffi::c_int;
        }
        // EmptyWorkingSet removes as many pages as possible from the working set
        // of the process, reclaiming unneeded memory back to the operating system
        // at phase boundaries and indexing checkpoints on Windows.
        unsafe {
            let process = GetCurrentProcess();
            EmptyWorkingSet(process);
        }
    }
}
