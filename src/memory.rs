//! Keeps the process's resident memory from sitting at its peak on glibc.
//!
//! glibc gives each thread that allocates its own memory pool (an arena), and
//! it rarely hands freed memory back to the system. The board's reader, the
//! queue's reader and the eval tab's load all run on background threads, so
//! one large read leaves the whole UI resident at its highest point until the
//! session ends.
//!
//! Three calls fix that: [`cap_arenas`] limits how many pools there can be,
//! [`fix_thresholds`] keeps glibc from raising the size it hands back at, and
//! [`trim`] returns freed memory after a large read. All are glibc-only. On
//! macOS and on the musl Linux build (which has no `mallopt`), all are
//! empty functions, so callers never need a `cfg` of their own and those
//! targets keep the behaviour they had.

/// How many arenas glibc may create.
///
/// Small but not one: with a single arena every thread's allocation takes the
/// same lock, and the UI thread would wait on a reader mid-load. Four leaves
/// the UI, the readers and the load thread room to run mostly apart while
/// still bounding the pools that each keep their own freed memory.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const ARENA_MAX: libc::c_int = 4;

/// Limit glibc's arena count. Call once, at the top of `main`, before any
/// thread starts: an arena that already exists is not taken back.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn cap_arenas() {
    // SAFETY: `mallopt` only sets an allocator parameter, and glibc takes
    // its arena lock to do it, so it is safe from any thread. Running before
    // any thread starts is what gives it effect, not what makes it safe. A
    // refusal (return 0) leaves glibc's default, so it is ignored.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, ARENA_MAX);
    }
}

/// The size, in bytes, glibc's mmap and trim thresholds are held at: its own
/// starting value for both.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const THRESHOLD: libc::c_int = 128 * 1024;

/// Hold glibc's mmap and trim thresholds at their starting value.
///
/// Left alone, glibc raises both each time a large block is freed: the mmap
/// threshold up to 32 MB, the trim threshold to twice that. After the eval
/// tab's load frees one large buffer, the next ones come out of the load
/// thread's own arena instead of a mapping of their own, and that arena keeps
/// up to 64 MB free at its top. [`trim`] does not shrink the top of an arena
/// other than the main one, so on glibc 2.39 about 30 MB stayed resident
/// after every load that landed that way: `footprint`'s check that the UI is
/// back under budget after leaving Eval failed in about half its runs on CI.
/// Setting either value also turns the raising off.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn fix_thresholds() {
    // SAFETY: as in `cap_arenas`, `mallopt` only sets an allocator parameter
    // under glibc's own lock. A refusal leaves glibc's default and is ignored.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, THRESHOLD);
        libc::mallopt(libc::M_TRIM_THRESHOLD, THRESHOLD);
    }
}

/// Give freed memory back to the system, across every arena.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn trim() {
    // SAFETY: `malloc_trim` is safe to call from any thread at any time.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn cap_arenas() {}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn fix_thresholds() {}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn trim() {}

#[cfg(test)]
mod tests {
    /// Every call is a plain function call on every target, and the process
    /// keeps allocating normally after them.
    #[test]
    fn trim_cap_and_thresholds_leave_the_allocator_usable() {
        super::cap_arenas();
        super::fix_thresholds();
        let big = vec![7u8; 8 << 20];
        drop(big);
        super::trim();
        assert_eq!(vec![1u8; 16].len(), 16);
    }
}
