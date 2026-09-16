pub fn reclaim_unused_pages() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        // glibc synchronizes access to all arenas, so this runs only at phase
        // boundaries where no worker is allocating: a trim of a large heap
        // takes hundreds of milliseconds and stalls every other thread.
        unsafe { malloc_trim(0) };
    }
}
