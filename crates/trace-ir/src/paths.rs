//! Filesystem path helpers shared by all pipeline stages.

use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

std::thread_local! {
    static CANON_CACHE: RefCell<FxHashMap<PathBuf, PathBuf>> =
        RefCell::new(FxHashMap::default());
}

/// Canonicalize `path`, falling back to the original path on error, and strip
/// the Windows extended-length prefix (`\\?\C:\...`,
/// `\\?\UNC\server\share\...`) that `std::fs::canonicalize` adds. Without the
/// strip, every interned file name in the IR and the exported database would
/// carry the prefix on Windows, breaking display and substring matching.
///
/// Repeat lookups of the same path skip `std::fs::canonicalize` (indexing
/// recanonicalizes include-graph keys per TU).
pub fn canonicalize(path: &Path) -> PathBuf {
    CANON_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(existing) = cache.get(path) {
            return existing.clone();
        }
        let canonical = canonicalize_uncached(path);
        cache.insert(path.to_path_buf(), canonical.clone());
        if canonical.as_path() != path {
            cache
                .entry(canonical.clone())
                .or_insert_with(|| canonical.clone());
        }
        canonical
    })
}

fn canonicalize_uncached(path: &Path) -> PathBuf {
    let canonical = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return path.to_path_buf(),
    };
    #[cfg(windows)]
    {
        let text = canonical.as_os_str().to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    canonical
}

/// Bumped by [`start_file_probe_epoch`]; a thread whose memo carries an older
/// one empties it before answering.
static PROBE_EPOCH: AtomicU64 = AtomicU64::new(0);

std::thread_local! {
    /// The epoch this memo was filled in, and encoded path bytes →
    /// `Path::is_file`. Keyed on the bytes rather than on a `PathBuf` because
    /// `Path`'s `Hash` walks the path a component at a time, normalizing
    /// separators as it goes, which on these paths costs more than hashing
    /// them flat.
    static PROBE_CACHE: RefCell<(u64, FxHashMap<Vec<u8>, bool>)> =
        RefCell::new((0, FxHashMap::default()));
}

/// Release filesystem memo storage owned by the calling thread after a phase.
/// Subsequent lookups repopulate it without invalidating other threads' caches.
pub fn release_thread_path_caches() {
    CANON_CACHE.with(|cache| *cache.borrow_mut() = FxHashMap::default());
    PROBE_CACHE.with(|cache| cache.borrow_mut().1 = FxHashMap::default());
}

/// Discard every memoized file probe: the tree may have changed since the last
/// one was taken.
///
/// An indexing run calls this before it reads anything, which is what bounds
/// [`is_file_cached`] — a stale answer cannot outlive one run, and neither can
/// the memory the memo holds. A long-lived host (the C API indexes repeatedly
/// in one process) therefore sees the tree as it is at the start of each run.
pub fn start_file_probe_epoch() {
    PROBE_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Generation of filesystem-derived caches for the current indexing run.
#[must_use]
pub fn file_probe_epoch() -> u64 {
    PROBE_EPOCH.load(Ordering::Relaxed)
}

/// Whether `path` names a regular file, memoized per thread for the current
/// epoch (see [`start_file_probe_epoch`]).
///
/// The answer is a function of the tree under analysis, which no stage writes
/// to, so a repeat probe of the same path can skip the `stat`. Include
/// resolution builds a candidate per (including file, spelling) pair and
/// probes it on every `#include` a unit executes, so the same handful of
/// candidates is probed over and over: camera resolves 4,343,229 candidates
/// naming 224,945 distinct paths, and the 95% that repeat were most of the
/// preprocessor's serial discovery pass (#83).
///
/// Within one epoch a caller that creates a file it has already asked about
/// still gets the old answer, so a caller doing that must probe the
/// filesystem directly.
pub fn is_file_cached(path: &Path) -> bool {
    let epoch = PROBE_EPOCH.load(Ordering::Relaxed);
    let key = path.as_os_str().as_encoded_bytes();
    PROBE_CACHE.with(|cache| {
        {
            let mut cache = cache.borrow_mut();
            if cache.0 != epoch {
                cache.0 = epoch;
                cache.1 = FxHashMap::default();
            } else if let Some(hit) = cache.1.get(key) {
                return *hit;
            }
        }
        let found = probe_file(path);
        cache.borrow_mut().1.insert(key.to_vec(), found);
        found
    })
}

/// What one directory holds, read once per epoch: every entry name as its
/// encoded bytes, and the same names ASCII-lowercased so a case-folding
/// filesystem's hits are not mistaken for misses.
struct DirListing {
    names: FxHashSet<Vec<u8>>,
    folded: FxHashSet<Vec<u8>>,
}

/// Epoch and encoded directory path → its listing, or `None` when the
/// directory could not be read in full (then every probe under it asks the
/// filesystem, as before). Shared by every thread: the discovery workers and
/// the other parallel phases walk the same search lists.
static DIR_LISTINGS: LazyLock<RwLock<DirListings>> =
    LazyLock::new(|| RwLock::new((0, FxHashMap::default())));

type DirListings = (u64, FxHashMap<Vec<u8>, Option<Arc<DirListing>>>);

/// `Path::is_file` for a first probe, answered from the parent directory's
/// listing whenever that settles it.
///
/// Include resolution probes a candidate per search directory until one
/// exists, so nearly every probe is a miss and the same few hundred
/// directories are asked about thousands of distinct names: camera's
/// 224,945 distinct candidates cost a `stat` each even after the memo above
/// (#83). A name the listing does not hold is not a file there, and the
/// listing is read once per directory per epoch. The filesystem still has
/// the last word wherever it might disagree with a byte comparison: a
/// listed name is confirmed a regular file with `stat`, a name that matches
/// only case-insensitively or is not ASCII (case-folding and
/// normalization-insensitive filesystems) is probed as before, and so is
/// one whose directory could not be listed.
fn probe_file(path: &Path) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return path.is_file();
    };
    if parent.as_os_str().is_empty() {
        return path.is_file();
    }
    let Some(listing) = dir_listing(parent) else {
        return path.is_file();
    };
    let name = name.as_encoded_bytes();
    if listing.names.contains(name)
        || !name.is_ascii()
        || listing.folded.contains(&name.to_ascii_lowercase())
    {
        return path.is_file();
    }
    false
}

fn dir_listing(dir: &Path) -> Option<Arc<DirListing>> {
    let epoch = PROBE_EPOCH.load(Ordering::Relaxed);
    let key = dir.as_os_str().as_encoded_bytes();
    if let Ok(cache) = DIR_LISTINGS.read() {
        if cache.0 == epoch {
            if let Some(listing) = cache.1.get(key) {
                return listing.clone();
            }
        }
    }
    let listing = read_listing(dir).map(Arc::new);
    let mut cache = DIR_LISTINGS.write().unwrap_or_else(|e| e.into_inner());
    if cache.0 != epoch {
        cache.0 = epoch;
        cache.1 = FxHashMap::default();
    }
    cache.1.entry(key.to_vec()).or_insert(listing).clone()
}

/// Every entry of `dir`, or `None` if any of it could not be read: a
/// partial listing would turn the files it missed into false misses.
fn read_listing(dir: &Path) -> Option<DirListing> {
    let mut names = FxHashSet::default();
    let mut folded = FxHashSet::default();
    for entry in std::fs::read_dir(dir).ok()? {
        let name = entry.ok()?.file_name();
        let bytes = name.as_encoded_bytes();
        folded.insert(bytes.to_ascii_lowercase());
        names.insert(bytes.to_vec());
    }
    Some(DirListing { names, folded })
}

/// Resolve `path` against `directory` and fold away `.` / `..` lexically before
/// canonicalizing, so an artifact that was never built still compares equal to
/// the same path spelled differently by another command.
pub fn resolve_against(directory: &Path, path: &Path) -> PathBuf {
    let joined = directory.join(path);
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push(component.as_os_str());
                }
            }
            part => normalized.push(part.as_os_str()),
        }
    }
    canonicalize(&normalized)
}

/// Directory-name policy for bare-tree production/test inference.
/// An empty effective list disables the partition. See docs/ANALYSIS.md,
/// "Declaring-header eligibility".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TestPartition {
    directories: Vec<String>,
}

impl Default for TestPartition {
    fn default() -> Self {
        Self {
            directories: ["test", "tests", "mock", "mocks"]
                .map(str::to_owned)
                .to_vec(),
        }
    }
}

impl TestPartition {
    /// Interpret CLI/C API options: no names keeps defaults; supplied names
    /// replace them. Disabling and supplying names are mutually exclusive.
    pub fn from_options(disabled: bool, mut names: Vec<String>) -> Result<Self, String> {
        if disabled {
            return if names.is_empty() {
                Ok(Self {
                    directories: Vec::new(),
                })
            } else {
                Err(
                    "disabling the test partition conflicts with specifying custom test directories"
                        .into(),
                )
            };
        }
        if names.is_empty() {
            return Ok(Self::default());
        }
        for name in &names {
            if name.is_empty()
                || matches!(name.as_str(), "." | "..")
                || name.contains(['/', '\\', '\0'])
            {
                return Err(format!(
                    "test directory name must be a single literal directory component: {name:?}"
                ));
            }
        }
        // Stable deduplication also keeps the recorded effective list concise.
        // Record decisions before mutating names so the set can borrow strings.
        let mut seen = FxHashSet::default();
        let keep: Vec<bool> = names
            .iter()
            .map(|name| seen.insert(name.as_str()))
            .collect();
        let mut keep = keep.into_iter();
        names.retain(|_| keep.next().unwrap());
        Ok(Self { directories: names })
    }

    pub fn directories(&self) -> &[String] {
        &self.directories
    }

    pub fn enabled(&self) -> bool {
        !self.directories.is_empty()
    }

    fn matches(&self, name: &std::ffi::OsStr) -> bool {
        self.directories
            .iter()
            .any(|dir| name == std::ffi::OsStr::new(dir))
    }
}

/// Bare-tree fallback partition; explicit include/link evidence takes precedence.
/// See docs/ANALYSIS.md, "Declaring-header eligibility".
///
/// True when a directory component of `path` below `root` matches the
/// configured policy (the file name itself does not count).
///
/// Precondition: `root` and `path` are spelled the same way -- both
/// canonical, as every pipeline stage passes them. The test is lexical: a
/// symlinked prefix or a `root` spelled differently from `path`'s prefix
/// yields `false`, and so does a path not under `root`. A `..` component below
/// `root` is folded lexically (`root/test/../src/a.h` is production); one that
/// climbs out of `root` yields `false`. Only that rare spelling allocates.
pub fn is_test_path(root: &Path, path: &Path, policy: &TestPartition) -> bool {
    if !policy.enabled() {
        return false;
    }
    let Some(parent) = path.strip_prefix(root).ok().and_then(Path::parent) else {
        return false;
    };
    let mut test = false;
    for part in parent.components() {
        match part {
            Component::Normal(name) => test |= policy.matches(name),
            Component::ParentDir => return is_test_dir_folded(parent, policy),
            _ => {}
        }
    }
    test
}

/// [`is_test_path`] for a relative directory spelled with `..`.
fn is_test_dir_folded(relative: &Path, policy: &TestPartition) -> bool {
    let mut dirs = Vec::new();
    for part in relative.components() {
        match part {
            Component::Normal(name) => dirs.push(name),
            Component::ParentDir if dirs.pop().is_none() => return false,
            _ => {}
        }
    }
    dirs.into_iter().any(|name| policy.matches(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_reads_directories_below_the_root() {
        let root = Path::new("/r");
        for (path, expected) in [
            ("/r/src/a.h", false),
            ("/r/test/a.h", true),
            ("/r/src/mocks/deep/a.cpp", true),
            ("/r/test.h", false),
            ("/r/src/test", false),
            ("/elsewhere/test/a.h", false),
            ("/r/test/../src/a.h", false),
            ("/r/src/../mock/a.h", true),
            ("/r/../test/a.h", false),
        ] {
            assert_eq!(
                is_test_path(root, Path::new(path), &TestPartition::default()),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn custom_partition_names_preserve_path_boundaries() {
        let policy = TestPartition::from_options(false, vec!["fakes".into()]).unwrap();
        let disabled = TestPartition::from_options(true, vec![]).unwrap();
        // The root's own name never classifies its contents as tests.
        let root = Path::new("/fakes");
        for (path, expected) in [
            ("/fakes/src/a.h", false),
            ("/fakes/mock/a.h", false),
            ("/fakes/fakes/a.h", true),
            ("/fakes/Fakes/a.h", false),
            ("/fakes/src/fakes", false),
            ("/fakes/fakes/../src/a.h", false),
            ("/fakes/src/../fakes/a.h", true),
            ("/fakes/../fakes/a.h", false),
            ("/elsewhere/fakes/a.h", false),
        ] {
            assert_eq!(
                is_test_path(root, Path::new(path), &policy),
                expected,
                "{path}"
            );
            assert!(!is_test_path(root, Path::new(path), &disabled), "{path}");
        }
    }

    #[test]
    fn review_relative_parents_survive_normalization() {
        for (base, path, expected) in [
            (
                ".",
                "../trace-unbuilt-review-artifact",
                "../trace-unbuilt-review-artifact",
            ),
            (
                "a",
                "../../trace-unbuilt-review-artifact",
                "../trace-unbuilt-review-artifact",
            ),
            (
                "..",
                "../trace-unbuilt-review-artifact",
                "../../trace-unbuilt-review-artifact",
            ),
            (
                "../a",
                "../trace-unbuilt-review-artifact",
                "../trace-unbuilt-review-artifact",
            ),
            (
                "/",
                "../trace-unbuilt-review-artifact",
                "/trace-unbuilt-review-artifact",
            ),
        ] {
            assert_eq!(
                resolve_against(Path::new(base), Path::new(path)),
                PathBuf::from(expected)
            );
        }
    }

    /// `PROBE_EPOCH` is process-global, so a test that bumps it empties the
    /// memo every other probe test is reading. Take this first.
    static EPOCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_new_epoch_forgets_what_the_tree_used_to_look_like() {
        let _guard = EPOCH.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let later = dir.path().join("appears-later.h");
        start_file_probe_epoch();
        assert!(!is_file_cached(&later));
        std::fs::write(&later, "int x;\n").unwrap();
        assert!(
            !is_file_cached(&later),
            "within one epoch the memo is what answers"
        );
        start_file_probe_epoch();
        assert!(is_file_cached(&later), "a new epoch re-reads the tree");
    }

    #[test]
    fn a_directory_listing_settles_only_what_the_filesystem_would() {
        let _guard = EPOCH.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Mixed.h"), "int x;\n").unwrap();
        std::fs::create_dir(dir.path().join("sub.h")).unwrap();
        start_file_probe_epoch();
        assert!(is_file_cached(&dir.path().join("Mixed.h")));
        assert!(!is_file_cached(&dir.path().join("absent.h")));
        // Listed, but a directory: the listing alone must not promote it.
        assert!(!is_file_cached(&dir.path().join("sub.h")));
        // Case is the filesystem's call, whichever way it goes here.
        for spelling in ["mixed.h", "MIXED.H"] {
            let p = dir.path().join(spelling);
            assert_eq!(is_file_cached(&p), p.is_file(), "{spelling}");
        }
        // An unreadable parent falls back to the plain probe.
        assert!(!is_file_cached(&dir.path().join("Mixed.h").join("inner.h")));
        assert!(!is_file_cached(&dir.path().join("nodir").join("inner.h")));
    }

    #[test]
    fn is_file_cached_answers_the_same_way_twice() {
        let _guard = EPOCH.lock().unwrap();
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/paths.rs");
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/no-such-file.rs");
        for _ in 0..2 {
            assert!(is_file_cached(&here), "{} is this file", here.display());
            assert!(
                !is_file_cached(&missing),
                "{} does not exist",
                missing.display()
            );
            // A directory is not a file, and the memo must not promote it to
            // one: include resolution probes directories all the time.
            assert!(!is_file_cached(here.parent().unwrap()));
        }
    }
    #[test]
    fn phase_release_drops_capacity_and_preserves_lookup_results() {
        let _guard = EPOCH.lock().unwrap();
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/paths.rs");
        let canonical = canonicalize(&here);
        assert!(is_file_cached(&here));
        release_thread_path_caches();
        CANON_CACHE.with(|cache| assert_eq!(cache.borrow().capacity(), 0));
        PROBE_CACHE.with(|cache| assert_eq!(cache.borrow().1.capacity(), 0));
        assert_eq!(canonicalize(&here), canonical);
        assert!(is_file_cached(&here));
    }
}
