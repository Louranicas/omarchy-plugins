//! Explicit-scope metadata search. No contents, shell, symlink traversal or implicit HOME.
//! Run on a worker: deadlines cannot interrupt a stalled kernel filesystem syscall.
use std::collections::{BTreeSet, BinaryHeap};
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Files,
    Repositories,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidRequest,
    Cancelled,
    UnsafeRoot,
    RootUnavailable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Incomplete {
    Deadline,
    EntryBudget,
    PathBudget,
    DepthBudget,
    Unreadable,
    UnsupportedName,
}

pub struct Request {
    pub generation: u64,
    pub query: String,
    pub roots: Vec<PathBuf>,
    pub mode: Mode,
    /// Legacy repository profile is 6; 0 removes this cap, not other budgets.
    pub repository_depth: usize,
    pub max_entries: usize,
    /// Maximum absolute logical output path bytes, including the configured root.
    pub max_path_bytes: usize,
    pub timeout: Duration,
    pub result_limit: usize,
    pub cross_mounts: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Identity {
    pub device: u64,
    pub inode: u64,
}
impl Identity {
    fn from_meta(m: &fs::Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Match {
    pub score: usize,
    pub path: PathBuf,
    pub root: PathBuf,
    pub root_identity: Identity,
    pub identity: Identity,
}
/// Matches are observations, never authority to open or execute a later path.
/// Consumers re-open beneath the pinned root and verify the full identity again.
#[derive(Debug)]
pub struct ResultSet {
    pub generation: u64,
    pub rows: Vec<Match>,
    pub total_matched: usize,
    pub capped: bool,
    pub complete: bool,
    pub disabled: bool,
    pub incomplete: BTreeSet<Incomplete>,
    pub examined: usize,
}

fn open_dir(parent: Option<&File>, path: &OsStr) -> io::Result<File> {
    let name = CString::new(path.as_bytes()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let fd = unsafe {
        libc::openat(
            parent.map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn root_dir(path: &Path) -> Result<File, Error> {
    let mut fd = open_dir(None, OsStr::new("/")).map_err(|_| Error::RootUnavailable)?;
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => {
                fd = open_dir(Some(&fd), name).map_err(|e| {
                    if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                        Error::UnsafeRoot
                    } else {
                        Error::RootUnavailable
                    }
                })?
            }
            _ => return Err(Error::UnsafeRoot),
        }
    }
    Ok(fd)
}

/// Legacy repo fallback ASCII compaction/subsequence score (not FFF scoring).
/// UTF-16 indices preserve JavaScript's index/length behavior for Unicode paths.
// ECMAScript WhiteSpace + LineTerminator set used by the pinned JS /\s/.
// Rust's is_whitespace also accepts U+0085 and does not accept U+FEFF.
fn js_space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}
pub fn repository_score(value: &str, query: &str) -> Option<usize> {
    let haystack = value.to_lowercase();
    let query = query.to_lowercase();
    if let Some(at) = haystack.find(&query) {
        return Some(haystack[..at].encode_utf16().count());
    }
    let needle: Vec<_> = query.chars().filter(|c| !js_space(*c)).collect();
    let compact: Vec<_> = haystack
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let mut at = 0;
    for c in &compact {
        if at < needle.len() && *c == needle[at] {
            at += 1;
        }
    }
    (at == needle.len()).then(|| 100 + compact.len() - needle.len())
}
pub fn strict_file_match(relative: &str, name: &str, query: &str) -> bool {
    let value = format!("{} {}", name.to_lowercase(), relative.to_lowercase());
    let terms: Vec<_> = query.split(js_space).filter(|s| !s.is_empty()).collect();
    !terms.is_empty()
        && terms
            .iter()
            .all(|term| value.contains(&term.to_lowercase()))
}

struct Scan<'a> {
    request: &'a Request,
    latest: &'a AtomicU64,
    deadline: Instant,
    result: ResultSet,
    matches: BinaryHeap<Match>,
    seen: BTreeSet<(u64, u64)>,
}
impl Scan<'_> {
    fn check(&mut self) -> Result<bool, Error> {
        if self.latest.load(Ordering::Acquire) != self.request.generation {
            return Err(Error::Cancelled);
        }
        if Instant::now() >= self.deadline {
            self.result.incomplete.insert(Incomplete::Deadline);
            return Ok(false);
        }
        if self.result.examined >= self.request.max_entries {
            self.result.incomplete.insert(Incomplete::EntryBudget);
            return Ok(false);
        }
        Ok(true)
    }
    fn candidate(&mut self, path: PathBuf, root: &Path, root_id: &Identity, meta: &fs::Metadata) {
        let Some(text) = path.to_str() else {
            self.result.incomplete.insert(Incomplete::UnsupportedName);
            return;
        };
        let score = match self.request.mode {
            Mode::Repositories => repository_score(text, &self.request.query),
            Mode::Files => {
                let relative = path
                    .strip_prefix(root)
                    .ok()
                    .and_then(Path::to_str)
                    .unwrap_or(text);
                let name = path.file_name().and_then(OsStr::to_str).unwrap_or("");
                strict_file_match(relative, name, &self.request.query).then_some(0)
            }
        };
        if let Some(score) = score {
            self.result.total_matched += 1;
            self.matches.push(Match {
                score,
                path,
                root: root.to_owned(),
                root_identity: root_id.clone(),
                identity: Identity::from_meta(meta),
            });
            if self.matches.len() > self.request.result_limit {
                self.matches.pop();
            }
        }
    }
    fn walk(
        &mut self,
        fd: File,
        root: &Path,
        root_id: &Identity,
        relative: &Path,
        depth: usize,
    ) -> Result<(), Error> {
        if !self.check()? {
            return Ok(());
        }
        let meta = fd.metadata().map_err(|_| Error::RootUnavailable)?;
        if !self.seen.insert((meta.dev(), meta.ino())) {
            return Ok(());
        }
        let pinned = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
        let entries = match fs::read_dir(&pinned) {
            Ok(entries) => entries,
            Err(_) => {
                self.result.incomplete.insert(Incomplete::Unreadable);
                return Ok(());
            }
        };
        for entry in entries {
            if !self.check()? {
                break;
            }
            self.result.examined += 1;
            let Ok(entry) = entry else {
                self.result.incomplete.insert(Incomplete::Unreadable);
                continue;
            };
            let name = entry.file_name();
            if name == ".cache" || name == "node_modules" {
                continue;
            }
            let next = relative.join(&name);
            if root.join(&next).as_os_str().as_bytes().len() > self.request.max_path_bytes {
                self.result.incomplete.insert(Incomplete::PathBudget);
                continue;
            }
            let Ok(info) = fs::symlink_metadata(pinned.join(&name)) else {
                self.result.incomplete.insert(Incomplete::Unreadable);
                continue;
            };
            if info.file_type().is_symlink() {
                continue;
            }
            if !self.request.cross_mounts && info.dev() != root_id.device {
                continue;
            }
            if name == ".git" {
                if self.request.mode == Mode::Repositories && (info.is_dir() || info.is_file()) {
                    self.candidate(root.join(relative), root, root_id, &meta);
                }
                continue;
            }
            if info.is_file() && self.request.mode == Mode::Files {
                self.candidate(root.join(&next), root, root_id, &info);
            } else if info.is_dir() {
                if self.request.mode == Mode::Repositories
                    && self.request.repository_depth != 0
                    && depth + 1 >= self.request.repository_depth
                {
                    continue;
                }
                if depth >= 127 {
                    self.result.incomplete.insert(Incomplete::DepthBudget);
                    continue;
                }
                match open_dir(Some(&fd), &name) {
                    Ok(child) => {
                        if child
                            .metadata()
                            .is_ok_and(|m| m.dev() == info.dev() && m.ino() == info.ino())
                        {
                            self.walk(child, root, root_id, &next, depth + 1)?;
                        } else {
                            self.result.incomplete.insert(Incomplete::Unreadable);
                        }
                    }
                    Err(_) => {
                        self.result.incomplete.insert(Incomplete::Unreadable);
                    }
                }
            }
        }
        Ok(())
    }
}

pub fn search(request: &Request, latest: &AtomicU64) -> Result<ResultSet, Error> {
    if request.generation == 0
        || request.roots.len() > 16
        || request.query.len() > 500
        || request.query.contains('\0')
        || request.query.trim_matches(js_space).is_empty()
        || request.repository_depth > 128
        || request.max_entries == 0
        || request.max_entries > 100_000
        || request.max_path_bytes == 0
        || request.max_path_bytes > 4096
        || request.result_limit == 0
        || request.result_limit > 100
        || request.timeout.is_zero()
        || request.timeout > Duration::from_secs(15)
        || request.roots.iter().any(|p| {
            !p.is_absolute()
                || p.as_os_str().as_bytes().len() > request.max_path_bytes
                || p.as_os_str().as_bytes().contains(&0)
                || p.components().any(|c| matches!(c, Component::ParentDir))
        })
    {
        return Err(Error::InvalidRequest);
    }
    let mut scan = Scan {
        request,
        latest,
        deadline: Instant::now() + request.timeout,
        result: ResultSet {
            generation: request.generation,
            rows: vec![],
            total_matched: 0,
            capped: false,
            complete: false,
            disabled: request.roots.is_empty(),
            incomplete: BTreeSet::new(),
            examined: 0,
        },
        matches: BinaryHeap::new(),
        seen: BTreeSet::new(),
    };
    for root in &request.roots {
        if !scan.check()? {
            break;
        }
        let fd = root_dir(root)?;
        let id = Identity::from_meta(&fd.metadata().map_err(|_| Error::RootUnavailable)?);
        scan.walk(fd, root, &id, Path::new(""), 0)?;
    }
    if latest.load(Ordering::Acquire) != request.generation {
        return Err(Error::Cancelled);
    }
    scan.result.capped = scan.result.total_matched > request.result_limit;
    scan.result.rows = scan.matches.into_sorted_vec();
    scan.result.complete = scan.result.incomplete.is_empty();
    Ok(scan.result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(root: &Path, mode: Mode) -> Request {
        Request {
            generation: 1,
            query: "repo".into(),
            roots: vec![root.into()],
            mode,
            repository_depth: 6,
            max_entries: 1000,
            max_path_bytes: 4096,
            timeout: Duration::from_secs(1),
            result_limit: 100,
            cross_mounts: false,
        }
    }
    fn run(r: &Request) -> ResultSet {
        search(r, &AtomicU64::new(1)).unwrap()
    }
    #[test]
    fn repository_directories_and_worktree_files_without_contents() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("repo/.git")).unwrap();
        fs::create_dir(t.path().join("repo-worktree")).unwrap();
        fs::write(
            t.path().join("repo-worktree/.git"),
            b"gitdir: /outside/sensitive",
        )
        .unwrap();
        let r = run(&request(t.path(), Mode::Repositories));
        assert!(r.complete);
        assert_eq!(r.rows.len(), 2);
    }
    #[test]
    fn empty_scope_disabled_and_stale_generation_discarded() {
        let mut r = request(Path::new("/"), Mode::Files);
        r.roots.clear();
        assert!(run(&r).disabled);
        assert_eq!(
            search(&r, &AtomicU64::new(2)).unwrap_err(),
            Error::Cancelled
        );
    }
    #[test]
    fn symlink_root_and_escape_are_never_followed() {
        let t = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("repo-secret"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), t.path().join("link")).unwrap();
        assert!(run(&request(t.path(), Mode::Files)).rows.is_empty());
        assert_eq!(
            search(
                &request(&t.path().join("link"), Mode::Files),
                &AtomicU64::new(1)
            )
            .unwrap_err(),
            Error::UnsafeRoot
        );
    }
    #[test]
    fn result_cap_is_separate_from_incomplete_scan() {
        let t = tempfile::tempdir().unwrap();
        for n in 0..105 {
            fs::write(t.path().join(format!("repo-{n:03}")), b"").unwrap();
        }
        let mut r = request(t.path(), Mode::Files);
        let out = run(&r);
        assert!(out.complete && out.capped);
        assert_eq!(out.rows.len(), 100);
        assert_eq!(out.total_matched, 105);
        r.max_entries = 3;
        let out = run(&r);
        assert!(!out.complete);
        assert!(out.incomplete.contains(&Incomplete::EntryBudget));
    }
    #[test]
    fn depth_zero_respects_other_budgets_and_excludes_caches() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("repo/a/b/.git")).unwrap();
        fs::create_dir_all(t.path().join("node_modules/repo/.git")).unwrap();
        let mut r = request(t.path(), Mode::Repositories);
        r.repository_depth = 2;
        assert!(run(&r).rows.is_empty());
        r.repository_depth = 0;
        assert_eq!(run(&r).rows.len(), 1);
        r.max_entries = 1;
        assert!(!run(&r).complete);
    }
    #[test]
    fn scoring_matches_legacy_ascii_and_utf16_cases() {
        assert_eq!(repository_score("/Projects/repository", "repo"), Some(10));
        assert_eq!(repository_score("abc-def", "adf"), Some(103));
        assert_eq!(repository_score("👩/repo", "repo"), Some(3));
        assert_eq!(repository_score("abc", "zzz"), None);
        assert!(strict_file_match(
            "Reports/annual.png",
            "annual.png",
            "annual .png"
        ));
        assert!(!strict_file_match("playing/new/game", "game", ".png"));
    }
    #[test]
    fn marker_depth_is_inclusive_and_zero_removes_only_policy_limit() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".git")).unwrap();
        fs::create_dir_all(t.path().join("repo/.git")).unwrap();
        fs::create_dir_all(t.path().join("repo/deeper/.git")).unwrap();
        let mut r = request(t.path(), Mode::Repositories);
        r.query = t.path().file_name().unwrap().to_str().unwrap().into();
        r.repository_depth = 1;
        let out = run(&r);
        assert!(out.complete);
        assert_eq!(out.rows.len(), 1);
        r.repository_depth = 2;
        assert_eq!(run(&r).rows.len(), 2);
        r.repository_depth = 3;
        assert_eq!(run(&r).rows.len(), 3);
        r.repository_depth = 0;
        assert_eq!(run(&r).rows.len(), 3);
    }
    #[test]
    fn absolute_path_and_root_input_are_bounded() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("repo-long"), b"").unwrap();
        let mut r = request(t.path(), Mode::Files);
        r.max_path_bytes = t.path().as_os_str().as_bytes().len() + 5;
        let out = run(&r);
        assert!(!out.complete);
        assert!(out.rows.is_empty());
        assert!(out.incomplete.contains(&Incomplete::PathBudget));
        r.max_path_bytes = 2;
        assert_eq!(
            search(&r, &AtomicU64::new(1)).unwrap_err(),
            Error::InvalidRequest
        );
        r.max_path_bytes = 4096;
        r.roots = vec![PathBuf::from(format!("/{}", "x".repeat(4096)))];
        assert_eq!(
            search(&r, &AtomicU64::new(1)).unwrap_err(),
            Error::InvalidRequest
        );
        r.roots = vec![PathBuf::from(OsStr::from_bytes(b"/bad\0path"))];
        assert_eq!(
            search(&r, &AtomicU64::new(1)).unwrap_err(),
            Error::InvalidRequest
        );
    }
    #[test]
    fn ecmascript_unicode_whitespace_and_utf16_scoring() {
        assert_eq!(repository_score("alpha_beta", "al\u{feff}be"), Some(105));
        assert_eq!(repository_score("alpha_beta", "al\u{0085}be"), None);
        assert!(strict_file_match(
            "alpha_beta",
            "alpha_beta",
            "alpha\u{feff}beta"
        ));
        assert!(!strict_file_match(
            "alpha_beta",
            "alpha_beta",
            "alpha\u{0085}beta"
        ));
        assert_eq!(repository_score("👩‍💻/repo", "repo"), Some(6));
        assert_eq!(repository_score("İ/repo", "repo"), Some(3));
    }
    #[test]
    fn matches_are_deterministic_observations_and_non_utf8_is_incomplete() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("repo-z"), b"").unwrap();
        fs::write(t.path().join("repo-a"), b"").unwrap();
        fs::write(t.path().join(OsStr::from_bytes(b"repo-\xff")), b"").unwrap();
        let mut r = request(t.path(), Mode::Files);
        r.result_limit = 1;
        let out = run(&r);
        assert!(out.capped);
        assert!(!out.complete);
        assert!(out.incomplete.contains(&Incomplete::UnsupportedName));
        assert_eq!(out.rows[0].path.file_name().unwrap(), "repo-a");
        let old = out.rows[0].identity.clone();
        // Keep original inode alive so replacement cannot immediately recycle it.
        let held = File::open(&out.rows[0].path).unwrap();
        fs::remove_file(&out.rows[0].path).unwrap();
        fs::write(&out.rows[0].path, b"replacement").unwrap();
        assert_ne!(
            Identity::from_meta(&fs::metadata(&out.rows[0].path).unwrap()),
            old
        );
        drop(held);
    }
}
