//! Getting files off a phone whose replies can't be trusted to match requests.
//!
//! iPhones over MTP can fall out of step and answer each request with the
//! reply to an earlier one: ask for B and you get A's bytes (or a fragment of
//! protocol data). So a download is never assumed to be the file that was
//! requested. Instead it is matched against the files requested recently, by
//! exact size *and* file format, and stored only if exactly one of them fits.
//! Anything else is thrown away, and files whose data never turned up stay
//! missing until a later pass.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::library::{
    Cancelled, Download, Library, Outcome, PhoneItem, PhoneSource, is_session_lost,
};

/// How many outstanding requests a reply may belong to: tolerates a phone that
/// is up to two replies behind.
const WINDOW: usize = 3;

/// Touched whenever the phone answers anything, so a watchdog can tell a slow
/// phone from a hung one.
#[derive(Clone)]
pub struct Heartbeat(Arc<(Instant, AtomicU64)>);

impl Heartbeat {
    pub fn new() -> Self {
        Self(Arc::new((Instant::now(), AtomicU64::new(0))))
    }

    pub fn beat(&self) {
        let ms = self.0.0.elapsed().as_millis() as u64;
        self.0.1.store(ms, Ordering::Relaxed);
    }

    /// Time since the last beat.
    pub fn idle(&self) -> Duration {
        let last = Duration::from_millis(self.0.1.load(Ordering::Relaxed));
        self.0.0.elapsed().saturating_sub(last)
    }
}

/// What a pass has stored so far, reported after every request.
#[derive(Debug, Default, Clone, Copy)]
pub struct Progress {
    pub copied: usize,
    pub duplicates: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassResult {
    pub copied: usize,
    pub duplicates: usize,
    /// Downloads that matched no requested file (or more than one), discarded.
    pub rejected: usize,
    /// Requested files whose data never turned up; retried on the next pass.
    pub unresolved: usize,
    /// The phone dropped the session part-way; reopen it and run another pass.
    pub session_lost: bool,
    /// The file being requested when the connection seemed lost.
    pub lost_on: Option<String>,
}

impl PassResult {
    pub fn progress(&self) -> usize {
        self.copied + self.duplicates
    }
}

struct Pass<'a, 'b> {
    library: &'a mut Library,
    source: &'a mut dyn PhoneSource,
    cancel: &'a AtomicBool,
    /// Requested, not yet matched, oldest first.
    pending: VecDeque<&'b PhoneItem>,
    /// Ids that were part of an ambiguous match.
    ambiguous: HashSet<&'b str>,
    result: PassResult,
}

/// Requests every item once and stores the replies that can be attributed.
/// Returns early (with `session_lost`) if the phone drops the connection.
/// Errors are only cancellation or failures writing to the destination.
pub fn run_pass(
    library: &mut Library,
    source: &mut dyn PhoneSource,
    items: &[&PhoneItem],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<PassResult> {
    let mut pass = Pass {
        library,
        source,
        cancel,
        pending: VecDeque::new(),
        ambiguous: HashSet::new(),
        result: PassResult::default(),
    };

    for item in items {
        progress(pass.progress());
        if !pass.request(item)? {
            return Ok(pass.finish());
        }
    }

    // A phone that is behind still owes replies for the last requests: asking
    // for them again shakes those replies loose.
    for _ in 0..WINDOW {
        let Some(item) = pass.pending.front().copied() else {
            break;
        };
        let before = pass.pending.len();
        if !pass.request(item)? {
            return Ok(pass.finish());
        }
        if pass.pending.len() == before && pass.pending.front().is_some_and(|p| p.id == item.id) {
            // Still unmatched: move on to the next one.
            pass.pending.rotate_left(1);
        }
    }

    // Files that only ever matched ambiguously (two pending files of the same
    // size and format): ask for each one alone, three times in a row. With the
    // phone at most one reply behind, the last two replies are this file's, so
    // they must agree.
    let ambiguous: Vec<&PhoneItem> = pass
        .pending
        .iter()
        .copied()
        .filter(|i| pass.ambiguous.contains(i.id.as_str()))
        .collect();
    for item in ambiguous {
        if !pass.isolate(item)? {
            return Ok(pass.finish());
        }
    }

    progress(pass.progress());
    Ok(pass.finish())
}

impl<'b> Pass<'_, 'b> {
    fn progress(&self) -> Progress {
        Progress {
            copied: self.result.copied,
            duplicates: self.result.duplicates,
        }
    }

    fn finish(mut self) -> PassResult {
        self.result.unresolved += self.pending.len();
        self.result
    }

    /// Requests `item` and attributes whatever comes back. `Ok(false)` means the
    /// session is gone.
    fn request(&mut self, item: &'b PhoneItem) -> Result<bool> {
        if !self.pending.iter().any(|p| p.id == item.id) {
            self.pending.push_back(item);
            while self.pending.len() > WINDOW {
                self.pending.pop_front();
                self.result.unresolved += 1;
            }
        }
        let Some(download) = self.download(item)? else {
            return Ok(!self.result.session_lost);
        };

        let matches: Vec<usize> = self
            .pending
            .iter()
            .enumerate()
            .filter(|(_, p)| fits(p, &download))
            .map(|(i, _)| i)
            .collect();
        match matches.as_slice() {
            [i] => {
                let matched = self.pending.remove(*i).expect("index from enumerate");
                if matched.id != item.id {
                    log::debug!("asked for {}, got {}", item.name, matched.name);
                }
                self.store(matched, download)?;
            }
            [] => {
                log::debug!(
                    "asked for {} ({} bytes), got {} unrecognised bytes",
                    item.name,
                    item.size,
                    download.size
                );
                self.library.discard(download);
                self.result.rejected += 1;
            }
            many => {
                for i in many {
                    self.ambiguous.insert(self.pending[*i].id.as_str());
                }
                self.library.discard(download);
                self.result.rejected += 1;
            }
        }
        Ok(true)
    }

    fn isolate(&mut self, item: &'b PhoneItem) -> Result<bool> {
        let mut replies = Vec::new();
        for _ in 0..3 {
            match self.download(item)? {
                Some(d) => replies.push(d),
                None if self.result.session_lost => {
                    replies.into_iter().for_each(|d| self.library.discard(d));
                    return Ok(false);
                }
                None => {}
            }
        }
        let agreed =
            replies.len() == 3 && replies[1].sha256 == replies[2].sha256 && fits(item, &replies[2]);
        let keep = if agreed { replies.pop() } else { None };
        replies.into_iter().for_each(|d| self.library.discard(d));
        match keep {
            Some(d) => {
                self.pending.retain(|p| p.id != item.id);
                self.store(item, d)?;
            }
            None => self.result.rejected += 1,
        }
        Ok(true)
    }

    /// Downloads, turning phone errors into `None` (and noting a lost session).
    fn download(&mut self, item: &PhoneItem) -> Result<Option<Download>> {
        match self.library.download(self.source, item, self.cancel) {
            Ok(d) => Ok(Some(d)),
            Err(e) if e.is::<Cancelled>() => Err(e),
            Err(e) if is_session_lost(&e) => {
                log::info!("{}: {e:#}", item.name);
                self.result.session_lost = true;
                self.result.lost_on = Some(item.id.clone());
                Ok(None)
            }
            Err(e) => {
                log::debug!("{}: {e:#}", item.name);
                Ok(None)
            }
        }
    }

    fn store(&mut self, item: &PhoneItem, download: Download) -> Result<()> {
        // An abandoned (hung, then woken) session must not write anything.
        if self.cancel.load(Ordering::Relaxed) {
            self.library.discard(download);
            return Err(Cancelled.into());
        }
        match self.library.store(item, download)? {
            Outcome::Copied(rel) => {
                log::info!("copied {} -> {}", item.name, rel.display());
                self.result.copied += 1;
            }
            Outcome::Duplicate(rel) => {
                log::info!("{} is already at {}", item.name, rel.display());
                self.result.duplicates += 1;
            }
            #[cfg(test)]
            Outcome::AlreadyImported => {}
        }
        Ok(())
    }
}

/// Whether `download` can be the content of `item`.
fn fits(item: &PhoneItem, download: &Download) -> bool {
    download.size == item.size && looks_like(&item.name, &download.head)
}

/// Checks the first bytes of a file against the format its extension promises.
/// Unknown extensions are accepted (the exact size still has to match).
pub(crate) fn looks_like(name: &str, head: &[u8]) -> bool {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    let starts = |magic: &[u8]| head.starts_with(magic);
    let brand = || (head.len() >= 12 && &head[4..8] == b"ftyp").then(|| &head[8..12]);
    match ext.as_str() {
        "jpg" | "jpeg" => starts(&[0xFF, 0xD8, 0xFF]),
        "png" => starts(&[0x89, b'P', b'N', b'G']),
        "gif" => starts(b"GIF8"),
        "dng" | "tif" | "tiff" => starts(b"II*\0") || starts(b"MM\0*"),
        "webp" => starts(b"RIFF") && head.get(8..12) == Some(b"WEBP"),
        "aae" | "plist" | "xml" => starts(b"<?xml") || starts(b"bplist"),
        "heic" | "heif" | "hif" | "avif" => brand().is_some_and(|b| {
            [
                b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"mif1", b"msf1", b"avif",
            ]
            .iter()
            .any(|x| b == *x)
        }),
        "mov" | "mp4" | "m4v" | "3gp" => match brand() {
            Some(b) => !matches!(b, b"heic" | b"heix" | b"mif1" | b"msf1" | b"avif"),
            None => head.len() >= 8 && matches!(&head[4..8], b"moov" | b"wide" | b"mdat" | b"free"),
        },
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;
    use std::io::{Cursor, Read};
    use std::path::{Path, PathBuf};

    use crate::library::{Listing, PhoneFolder, SessionLost};

    fn heic(seed: u8, len: usize) -> Vec<u8> {
        let mut v = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic".to_vec();
        v.resize(len, seed);
        v
    }

    fn mov(seed: u8, len: usize) -> Vec<u8> {
        let mut v = b"\0\0\0\x14ftypqt  \0\0\0\0qt  ".to_vec();
        v.resize(len, seed);
        v
    }

    fn jpg(seed: u8, len: usize) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE1];
        v.resize(len, seed);
        v
    }

    /// A phone that answers `lag` requests behind, optionally dropping the
    /// session after a number of requests.
    struct BuggyPhone {
        files: Vec<(PhoneItem, Vec<u8>)>,
        lag: usize,
        replies: VecDeque<Vec<u8>>,
        drop_after: Option<usize>,
        requests: usize,
    }

    impl BuggyPhone {
        fn new(lag: usize, files: Vec<(&str, Vec<u8>)>) -> Self {
            let files = files
                .into_iter()
                .enumerate()
                .map(|(i, (name, data))| {
                    let item = PhoneItem {
                        id: format!("o{i}"),
                        name: name.into(),
                        size: data.len() as u64,
                        month: Some((2024, 3)),
                        video: false,
                    };
                    (item, data)
                })
                .collect();
            Self {
                files,
                lag,
                replies: VecDeque::new(),
                drop_after: None,
                requests: 0,
            }
        }

        fn items(&self) -> Vec<PhoneItem> {
            self.files.iter().map(|(i, _)| i.clone()).collect()
        }

        /// Simulates unplugging and reopening: a fresh session, same lag.
        fn reopen(&mut self) {
            self.replies.clear();
            self.drop_after = None;
            self.requests = 0;
        }
    }

    impl PhoneSource for BuggyPhone {
        fn root(&mut self) -> Result<PhoneFolder> {
            Ok(PhoneFolder {
                id: "root".into(),
                name: "root".into(),
            })
        }

        fn list(&mut self, _folder: &PhoneFolder) -> Result<Listing> {
            Ok(Listing {
                files: BuggyPhone::items(self),
                ..Listing::default()
            })
        }

        fn open(&mut self, item: &PhoneItem) -> Result<Box<dyn Read + '_>> {
            self.requests += 1;
            if self.drop_after.is_some_and(|n| self.requests > n) {
                return Err(SessionLost("test".into()).into());
            }
            let data = self
                .files
                .iter()
                .find(|(i, _)| i.id == item.id)
                .unwrap()
                .1
                .clone();
            self.replies.push_back(data);
            let reply = if self.replies.len() > self.lag {
                self.replies.pop_front().unwrap()
            } else {
                // What a lagging phone sends before it has anything queued:
                // a scrap of protocol data.
                vec![
                    4, 0, 0, 0, 0x21, 0x79, 0, 0, 0, 0x79, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8,
                ]
            };
            Ok(Box::new(Cursor::new(reply)))
        }
    }

    struct Env {
        _dir: tempfile::TempDir,
        root: PathBuf,
        state: PathBuf,
    }

    impl Env {
        fn dest(&self) -> crate::library::Destinations {
            crate::library::Destinations {
                photos: self.root.clone(),
                videos: None,
            }
        }
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        Env {
            root: dir.path().join("Photos"),
            state: dir.path().join("library.json"),
            _dir: dir,
        }
    }

    fn pass(env: &Env, phone: &mut BuggyPhone) -> PassResult {
        let mut lib = Library::open(&env.dest(), &env.state).unwrap();
        let items = BuggyPhone::items(phone);
        let todo: Vec<_> = items.iter().filter(|i| !lib.is_imported(i)).collect();
        let r = run_pass(&mut lib, phone, &todo, &AtomicBool::new(false), &mut |_| {}).unwrap();
        lib.finish().unwrap();
        r
    }

    /// Every stored file holds exactly the bytes of the phone file it is named after.
    fn assert_all_correct(env: &Env, phone: &BuggyPhone) {
        let folder = env.root.join("2024").join("03");
        for (item, data) in &phone.files {
            let path = folder.join(&item.name);
            assert_eq!(&fs::read(&path).unwrap(), data, "{}", item.name);
        }
        let stored = fs::read_dir(&folder).unwrap().count();
        assert_eq!(stored, phone.files.len());
    }

    fn sample() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("IMG_0001.HEIC", heic(1, 1000)),
            ("IMG_0002.MOV", mov(2, 1300)),
            ("IMG_0003.HEIC", heic(3, 900)),
            ("IMG_0004.JPG", jpg(4, 700)),
            ("IMG_0005.HEIC", heic(5, 1100)),
        ]
    }

    #[test]
    fn in_step_phone() {
        let env = env();
        let mut phone = BuggyPhone::new(0, sample());
        let r = pass(&env, &mut phone);
        assert_eq!((r.copied, r.rejected, r.unresolved), (5, 0, 0));
        assert_all_correct(&env, &phone);
    }

    #[test]
    fn phone_one_reply_behind() {
        let env = env();
        let mut phone = BuggyPhone::new(1, sample());
        let r = pass(&env, &mut phone);
        assert_eq!((r.copied, r.unresolved), (5, 0));
        assert_all_correct(&env, &phone);
    }

    #[test]
    fn phone_two_replies_behind() {
        let env = env();
        let mut phone = BuggyPhone::new(2, sample());
        let r = pass(&env, &mut phone);
        assert_eq!((r.copied, r.unresolved), (5, 0));
        assert_all_correct(&env, &phone);
    }

    #[test]
    fn same_size_same_format_is_never_guessed() {
        // Two different photos of identical size in a row, phone one behind.
        let env = env();
        let mut phone = BuggyPhone::new(
            1,
            vec![
                ("IMG_0001.HEIC", heic(1, 1000)),
                ("IMG_0002.HEIC", heic(2, 1000)),
                ("IMG_0003.JPG", jpg(3, 500)),
            ],
        );
        let r = pass(&env, &mut phone);
        assert_eq!(r.copied, 3, "{r:?}");
        assert_all_correct(&env, &phone);
    }

    #[test]
    fn dropped_session_resumes_on_the_next_pass() {
        let env = env();
        let mut phone = BuggyPhone::new(1, sample());
        phone.drop_after = Some(3);
        let first = pass(&env, &mut phone);
        assert!(first.session_lost);
        assert!(first.copied < 5);

        phone.reopen();
        let second = pass(&env, &mut phone);
        assert!(!second.session_lost);
        assert_eq!(first.copied + second.copied, 5);
        assert_all_correct(&env, &phone);
    }

    #[test]
    fn mislabelled_data_is_rejected() {
        // The phone lists sizes that match nothing it actually sends.
        let env = env();
        let mut phone = BuggyPhone::new(0, sample());
        for (item, _) in &mut phone.files {
            item.size += 7;
        }
        let r = pass(&env, &mut phone);
        assert_eq!(r.copied, 0);
        assert!(r.rejected >= 5);
        assert!(!Path::new(&env.root.join("2024")).exists());
        assert_eq!(fs::read_dir(&env.root).unwrap().count(), 0);
    }

    #[test]
    fn recognises_formats() {
        assert!(looks_like("a.HEIC", &heic(0, 32)));
        assert!(!looks_like("a.HEIC", &mov(0, 32)));
        assert!(looks_like("a.MOV", &mov(0, 32)));
        assert!(!looks_like("a.MOV", &heic(0, 32)));
        assert!(looks_like("a.jpg", &jpg(0, 32)));
        assert!(!looks_like("a.png", &jpg(0, 32)));
        assert!(looks_like("a.AAE", b"<?xml version=\"1.0\""));
        assert!(!looks_like("a.AAE", &heic(0, 32)));
        assert!(looks_like("a.unknown", b"anything"));
    }
}
