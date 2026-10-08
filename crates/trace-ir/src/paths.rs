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

/// Release both thread-local path memos and shared directory listings.
pub fn release_filesystem_caches() {
    release_thread_path_caches();
    if let Ok(mut cache) = DIR_LISTINGS.write() {
        *cache = (0, FxHashMap::default());
    }
    if let Ok(mut absent) = ABSENT_SEARCH_DIRS.write() {
        *absent = (0, FxHashSet::default());
    }
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

/// Whether `path` names a regular file. Answered from the parent's directory
/// listing whenever that settles it, and otherwise asked of the filesystem
/// and memoized per thread for the current epoch (see
/// [`start_file_probe_epoch`]); a miss a listing settles is not memoized.
///
/// The answer is a function of the tree under analysis, which no stage writes
/// to, so a repeat probe of the same path can skip the `stat`. Include
/// resolution builds a candidate per (including file, spelling) pair and
/// probes it on every `#include` a unit executes, so the same handful of
/// candidates is probed over and over: camera resolves 4,343,229 candidates
/// naming 224,945 distinct paths, and the 95% that repeat were most of the
/// preprocessor's serial discovery pass (#83).
///
/// Within one epoch the answers are snapshots: a file created after it was
/// asked about, or after its directory was listed, is not seen, so a caller
/// doing that must probe the filesystem directly. Only what no memo entry
/// and no listing covers yet is read fresh.
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
        match probe_file(path) {
            Probe::Miss => false,
            Probe::Stat(found) => {
                cache.borrow_mut().1.insert(key.to_vec(), found);
                found
            }
        }
    })
}

/// [`is_file_cached`] for a candidate of an include search: whether `name`,
/// the spelling an `#include` names, is a regular file under the search
/// directory `dir`.
///
/// A spelling is tried under every search directory, and nearly every
/// candidate names a subdirectory that is not there (`llvm/ADT/Foo.h` under
/// each of thousands of directories). The search directory is read and
/// recorded on its first probe, so each such candidate is settled from that
/// listing by the child it does not hold, with no `read_dir` per candidate
/// and no entry per absent directory ([`DIR_LISTINGS`]); a search directory
/// that is not there is remembered as such, once per epoch. Nothing above
/// the search directory is read.
pub fn is_file_in(dir: &Path, name: &Path) -> bool {
    let epoch = PROBE_EPOCH.load(Ordering::Relaxed);
    if !is_listed(dir, epoch) && !list_search_directory(dir, epoch) {
        // A miss for every spelling, one that leaves `dir` (`../a.h`)
        // included: the kernel resolves `missing/../a.h` a component at a
        // time, so the file beside an absent `dir` is not reachable through
        // it, as a compiler opening the same candidate would find.
        return false;
    }
    is_file_cached(&dir.join(name))
}

/// Search directories found absent, or not directories, this epoch: one
/// per `-I`, so bounded by the search lists, unlike the subdirectories the
/// spellings name under them.
static ABSENT_SEARCH_DIRS: LazyLock<RwLock<(u64, FxHashSet<Vec<u8>>)>> =
    LazyLock::new(|| RwLock::new((0, FxHashSet::default())));

/// Read and record the search directory `dir`; `false` when it is not there
/// (or is a regular file), which is remembered for the epoch.
fn list_search_directory(dir: &Path, epoch: u64) -> bool {
    let key = dir.as_os_str().as_encoded_bytes();
    {
        let absent = ABSENT_SEARCH_DIRS.read().unwrap_or_else(|e| e.into_inner());
        if absent.0 == epoch && absent.1.contains(key) {
            return false;
        }
    }
    let Some(listing) = listing_or_absent(dir) else {
        let mut absent = ABSENT_SEARCH_DIRS
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if absent.0 != epoch {
            *absent = (epoch, FxHashSet::default());
        }
        absent.1.insert(key.to_vec());
        return false;
    };
    record_listing(dir, listing, epoch);
    true
}

/// [`read_listing`] with its failures sorted: `None` when `dir` is not
/// there or is a regular file (the probe is a miss), `Some(None)` when it
/// is a directory that could not be read in full (every probe under it
/// asks the filesystem). An error of another kind is told apart by asking
/// whether `dir` is a directory, so a platform or mount reporting an absent
/// path as one does not leave an entry per absent directory behind.
fn listing_or_absent(dir: &Path) -> Option<Option<Arc<DirListing>>> {
    match read_listing(dir) {
        Ok(listing) => Some(listing.map(Arc::new)),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            None
        }
        Err(_) => dir.is_dir().then_some(None),
    }
}

/// Record `listing` for `dir` in this epoch, keeping an entry already there.
fn record_listing(
    dir: &Path,
    listing: Option<Arc<DirListing>>,
    epoch: u64,
) -> Option<Arc<DirListing>> {
    let mut cache = DIR_LISTINGS.write().unwrap_or_else(|e| e.into_inner());
    if cache.0 != epoch {
        cache.0 = epoch;
        cache.1 = FxHashMap::default();
    }
    cache
        .1
        .entry(dir.as_os_str().as_encoded_bytes().to_vec())
        .or_insert(listing)
        .clone()
}

/// How [`probe_file`] answered a first probe.
enum Probe {
    /// Settled by a directory listing: the name is not there. Not memoized,
    /// since the listings answer a repeat as cheaply (see [`DIR_LISTINGS`]).
    Miss,
    /// Asked of the filesystem; bounded by the files that exist, so worth
    /// memoizing.
    Stat(bool),
}

/// What one directory holds, read once per epoch: every entry name as its
/// encoded bytes, and the same names ASCII-lowercased so a case-folding
/// filesystem's hits are not mistaken for misses.
struct DirListing {
    names: FxHashSet<Vec<u8>>,
    folded: FxHashSet<Vec<u8>>,
}

impl DirListing {
    /// Whether `name` may be an entry: listed, or one a case-folding or
    /// normalization-insensitive filesystem could still match.
    fn may_hold(&self, name: &[u8]) -> bool {
        if self.names.contains(name) || !name.is_ascii() {
            return true;
        }
        // Folding a name with no uppercase is the identity: spare the
        // allocation on the hot miss path.
        if name.iter().any(u8::is_ascii_uppercase) {
            self.folded.contains(&name.to_ascii_lowercase())
        } else {
            self.folded.contains(name)
        }
    }
}

/// Epoch and encoded directory path → its listing, or `None` when the
/// directory could not be read in full (then every probe under it asks the
/// filesystem, as before). Shared by every thread: the discovery workers and
/// the other parallel phases walk the same search lists.
///
/// Holds only directories that exist and were probed into (a search
/// directory, by [`is_file_in`]), or lie under one that was: never an absent
/// directory, never a path through a regular file, and never an ancestor
/// nobody probed into. A spelling
/// `llvm/ADT/Foo.h` names a `llvm/ADT` under every search
/// directory it is tried in, nearly all of them absent; an entry per (search
/// directory × spelled subdirectory), and a memo entry per candidate, took
/// gigabytes on the LLVM monorepo before its include graph was built
/// (#209). An absent directory is instead settled from the nearest listing
/// in hand, by the child it does not hold on the way down
/// ([`settled_by_listings`]), and reading starts only below a listed
/// directory ([`read_down_to`]): reading ancestors on demand would snapshot
/// directories above the analyzed tree, and a tree created later in the
/// same epoch under such an ancestor -- one test's scratch directory after
/// another's -- would be invisible.
static DIR_LISTINGS: LazyLock<RwLock<DirListings>> =
    LazyLock::new(|| RwLock::new((0, FxHashMap::default())));

type DirListings = (u64, FxHashMap<Vec<u8>, Option<Arc<DirListing>>>);

/// `Path::is_file` for a first probe, answered from directory listings
/// whenever those settle it.
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
fn probe_file(path: &Path) -> Probe {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let (Some(parent), Some(name)) = (parent, path.file_name()) else {
        return Probe::Stat(path.is_file());
    };
    let name = name.as_encoded_bytes();
    let epoch = PROBE_EPOCH.load(Ordering::Relaxed);
    let ask = settled_by_listings(parent, name, epoch)
        .unwrap_or_else(|| read_down_to(parent, name, epoch));
    if ask {
        Probe::Stat(path.is_file())
    } else {
        Probe::Miss
    }
}

/// What the listings in hand say of `name` under `dir`, reading and
/// recording nothing: `Some(true)` when the filesystem must be asked (the
/// name is listed, or `dir` could not be listed), `Some(false)` when `dir`
/// or a directory on the way down to it is not there, `None` when `dir` must
/// be read first. One lock and one lookup per level for the whole walk up.
fn settled_by_listings(dir: &Path, name: &[u8], epoch: u64) -> Option<bool> {
    let cache = DIR_LISTINGS.read().unwrap_or_else(|e| e.into_inner());
    if cache.0 != epoch {
        return None;
    }
    let mut child = name;
    for ancestor in dir.ancestors() {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        if let Some(listing) = cache.1.get(ancestor.as_os_str().as_encoded_bytes()) {
            let held = listing
                .as_ref()
                .is_none_or(|listing| listing.may_hold(child));
            return match (held, ancestor == dir) {
                (false, _) => Some(false),
                (true, true) => Some(true),
                (true, false) => None,
            };
        }
        child = ancestor.file_name()?.as_encoded_bytes();
    }
    None
}

/// Read and record `dir` -- and first every directory between the nearest
/// listed ancestor and it, each of which exists, so a repeat probe below
/// them is settled from memory -- then whether the filesystem must be asked
/// about `name` under it. With no listed ancestor only `dir` itself is read.
/// A level `read_dir` reports absent, or a regular file rather than a
/// directory, settles the probe as a miss and is not recorded.
fn read_down_to(dir: &Path, name: &[u8], epoch: u64) -> bool {
    let mut levels = vec![dir];
    let mut below_listed = false;
    for ancestor in dir.ancestors().skip(1) {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        if is_listed(ancestor, epoch) {
            below_listed = true;
            break;
        }
        levels.push(ancestor);
    }
    if !below_listed {
        levels.truncate(1);
    }
    for (i, level) in levels.iter().enumerate().rev() {
        let child = if i == 0 {
            Some(name)
        } else {
            levels[i - 1].file_name().map(|n| n.as_encoded_bytes())
        };
        let Some(listing) = listing_or_absent(level) else {
            return false;
        };
        let listing = record_listing(level, listing, epoch);
        if let (Some(listing), Some(child)) = (listing, child) {
            if !listing.may_hold(child) {
                return false;
            }
        }
    }
    true
}

fn is_listed(dir: &Path, epoch: u64) -> bool {
    let cache = DIR_LISTINGS.read().unwrap_or_else(|e| e.into_inner());
    cache.0 == epoch && cache.1.contains_key(dir.as_os_str().as_encoded_bytes())
}

/// Every entry of `dir`; `None` when an entry could not be read, since a
/// partial listing would turn the files it missed into false misses; or the
/// error that kept the directory from being opened at all. Only the open
/// says whether the directory is there -- an entry's error (a stale entry
/// on a network mount, a removal race) must not.
fn read_listing(dir: &Path) -> std::io::Result<Option<DirListing>> {
    let entries = std::fs::read_dir(dir)?;
    let mut names = FxHashSet::default();
    let mut folded = FxHashSet::default();
    for entry in entries {
        let Ok(entry) = entry else {
            return Ok(None);
        };
        let bytes = entry.file_name();
        let bytes = bytes.as_encoded_bytes();
        folded.insert(bytes.to_ascii_lowercase());
        names.insert(bytes.to_vec());
    }
    Ok(Some(DirListing { names, folded }))
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

    /// Whether a directory called `name` puts what is below it in the partition.
    pub fn matches(&self, name: &std::ffi::OsStr) -> bool {
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

    fn probe_memo_len() -> usize {
        PROBE_CACHE.with(|cache| cache.borrow().1.len())
    }

    fn listed(dir: &Path) -> bool {
        is_listed(dir, file_probe_epoch())
    }

    /// Include resolution tries a spelling under every search directory, so
    /// nearly every candidate names a subdirectory that is not there. Those
    /// leave nothing behind: neither a memo entry per candidate nor a
    /// listing entry per absent directory -- both grew with (search
    /// directories × spellings) and exhausted memory on LLVM (#209). A
    /// directory that does exist below a listed one is read and recorded on
    /// the way down, so a repeat probe below it reads nothing.
    #[test]
    fn absent_directories_leave_no_memo_behind() {
        let _guard = EPOCH.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let inc = dir.path().join("inc");
        std::fs::create_dir_all(inc.join("sub")).unwrap();
        std::fs::write(inc.join("a.h"), "int x;\n").unwrap();
        start_file_probe_epoch();
        release_thread_path_caches();
        assert!(is_file_cached(&inc.join("a.h")), "lists inc");
        for spelled in ["nope/z.h", "llvm/ADT/x.h", "sub/deep/y.h"] {
            let candidate = inc.join(spelled);
            for _ in 0..2 {
                assert!(!is_file_cached(&candidate), "{}", candidate.display());
            }
        }
        for absent in ["nope", "llvm", "llvm/ADT", "sub/deep"] {
            assert!(!listed(&inc.join(absent)), "{absent} gets no entry");
        }
        assert!(listed(&inc.join("sub")), "an existing level is recorded");
        assert_eq!(
            probe_memo_len(),
            1,
            "only the file the filesystem was asked about is memoized"
        );
    }

    /// An include search probes a spelling under every search directory, so
    /// the search directory itself is listed once and every absent
    /// subdirectory a spelling names under it is settled from that listing:
    /// no `read_dir` per candidate, no entry per absent directory, and no
    /// memo entry for a miss. A search directory that is not there is
    /// remembered as such, once.
    #[test]
    fn a_search_directory_is_listed_once_for_every_spelling_under_it() {
        let _guard = EPOCH.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let inc = dir.path().join("inc");
        std::fs::create_dir_all(&inc).unwrap();
        std::fs::write(inc.join("a.h"), "int x;\n").unwrap();
        start_file_probe_epoch();
        release_thread_path_caches();
        for spelled in ["llvm/ADT/x.h", "llvm/ADT/y.h", "nope/z.h"] {
            for _ in 0..2 {
                assert!(!is_file_in(&inc, Path::new(spelled)), "{spelled}");
            }
        }
        assert!(listed(&inc), "the search directory is listed");
        for absent in ["llvm", "llvm/ADT", "nope"] {
            assert!(!listed(&inc.join(absent)), "{absent} gets no entry");
        }
        assert_eq!(probe_memo_len(), 0, "a settled miss is not memoized");
        assert!(is_file_in(&inc, Path::new("a.h")));
        let missing = dir.path().join("missing");
        for _ in 0..2 {
            assert!(!is_file_in(&missing, Path::new("llvm/ADT/x.h")));
        }
        assert!(!listed(&missing));
        assert!(
            !listed(dir.path()),
            "nothing above a search directory is read"
        );
    }

    /// A spelling that leaves an absent search directory (`../a.h` under
    /// `missing`) is a miss, as the filesystem resolves it: the kernel walks
    /// `missing` before `..`, so the file beside `missing` is not reachable
    /// through it. The early answer agrees with `Path::is_file`.
    #[test]
    fn an_escaping_spelling_under_an_absent_search_directory_is_a_miss() {
        let _guard = EPOCH.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.h"), "").unwrap();
        start_file_probe_epoch();
        release_thread_path_caches();
        let missing = dir.path().join("missing");
        let escaping = missing.join("../a.h");
        assert!(!escaping.is_file(), "{}", escaping.display());
        assert!(!is_file_in(&missing, Path::new("../a.h")));
        // Under a search directory that exists, the same spelling is found.
        let present = dir.path().join("present");
        std::fs::create_dir(&present).unwrap();
        assert!(is_file_in(&present, Path::new("../a.h")));
    }

    /// Only a listing a probe asked for directly is in hand: ancestors are
    /// never read to settle a child, so a tree created later in the same
    /// epoch next to one already probed -- as every test's scratch directory
    /// is -- is read when it is first asked about.
    #[test]
    fn a_sibling_tree_created_later_is_still_found() {
        let _guard = EPOCH.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        std::fs::create_dir(&first).unwrap();
        std::fs::write(first.join("a.h"), "").unwrap();
        start_file_probe_epoch();
        assert!(is_file_cached(&first.join("a.h")));
        assert!(!is_file_cached(&first.join("sub/b.h")));
        assert!(!listed(root.path()), "nothing probed into the root");
        let second = root.path().join("second");
        std::fs::create_dir(&second).unwrap();
        std::fs::write(second.join("c.h"), "").unwrap();
        assert!(is_file_cached(&second.join("c.h")));
    }

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
        // A path through a regular file is a miss, with no entry recorded
        // for the file as if it were a directory; an absent parent likewise.
        assert!(!is_file_cached(&dir.path().join("Mixed.h").join("inner.h")));
        assert!(!listed(&dir.path().join("Mixed.h")));
        assert!(!is_file_cached(&dir.path().join("nodir").join("inner.h")));
        assert!(!listed(&dir.path().join("nodir")));
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
