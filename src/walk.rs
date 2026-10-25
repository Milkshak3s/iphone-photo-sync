//! Walking the phone one folder at a time and copying each folder's new photos
//! before moving on, the way Explorer reveals a growing list. A run that is
//! interrupted keeps everything it already copied.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;

use crate::library::{
    Cancelled, Library, PhoneFolder, PhoneItem, PhoneSource, is_session_lost, mark_videos,
};
use crate::transfer;

/// Attempts at listing one folder before it counts as failed.
const LIST_ATTEMPTS: usize = 3;
/// Transfer passes per folder without progress before moving on.
const FOLDER_PASSES: usize = 3;
/// Files per folder that may fail with a "lost connection" error while the
/// phone in fact keeps answering, before the walk stops.
const MAX_SKIPS: usize = 10;

#[derive(Debug, Default, Clone)]
pub struct WalkProgress {
    pub folder: String,
    pub folders_done: usize,
    /// Folders found so far (grows as the walk discovers them).
    pub folders_known: usize,
    pub files_seen: usize,
    pub copied: usize,
    pub duplicates: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WalkReport {
    pub folders_done: usize,
    pub folders_failed: usize,
    pub files_seen: usize,
    /// Objects whose properties couldn't be read (often iCloud-only photos).
    pub unreadable: usize,
    pub copied: usize,
    pub duplicates: usize,
    /// Listed files still not on the PC at the end (videos only when a videos
    /// folder is set).
    pub missing: usize,
    /// Videos left alone because no videos folder is set.
    pub skipped_videos: usize,
    /// The phone dropped the connection part-way; the walk stopped there.
    pub session_lost: bool,
}

impl WalkReport {
    pub fn complete(&self) -> bool {
        self.missing == 0 && self.folders_failed == 0 && !self.session_lost
    }
}

/// Walks the phone breadth-first and copies everything missing. Fails only if
/// the phone's top folder can't be listed, on cancellation, or on errors
/// writing to the destination.
pub fn walk_and_copy(
    library: &mut Library,
    source: &mut dyn PhoneSource,
    cancel: &AtomicBool,
    retry_pause: Duration,
    progress: &mut dyn FnMut(&WalkProgress),
) -> Result<WalkReport> {
    let root = source.root()?;
    let mut queue = VecDeque::from([root]);
    let mut report = WalkReport::default();
    let mut state = WalkProgress {
        folders_known: 1,
        ..WalkProgress::default()
    };
    let mut first = true;

    while let Some(folder) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        state.folder = folder.name.clone();
        progress(&state);

        let mut listing = match list(source, &folder, cancel, retry_pause) {
            Ok(l) => l,
            Err(e) if e.is::<Cancelled>() => return Err(e),
            Err(e) if first => return Err(e),
            Err(e) if is_session_lost(&e) => {
                log::info!("{}: {e:#}", folder.name);
                report.session_lost = true;
                break;
            }
            Err(e) => {
                log::info!("couldn't list {}: {e:#}", folder.name);
                report.folders_failed += 1;
                continue;
            }
        };
        first = false;
        mark_videos(&mut listing.files);
        let files_here = listing.files.len();
        let (wanted, skipped): (Vec<PhoneItem>, Vec<PhoneItem>) =
            listing.files.into_iter().partition(|i| library.wants(i));
        report.skipped_videos += skipped.len();
        state.folders_known += listing.folders.len();
        state.files_seen += files_here;
        report.files_seen += files_here;
        report.unreadable += listing.unreadable;
        queue.extend(listing.folders);

        let lost = copy_folder(
            library,
            source,
            &folder,
            &wanted,
            cancel,
            &mut report,
            &mut state,
            progress,
        )?;
        library.save()?;
        if lost {
            report.session_lost = true;
            break;
        }
        report.missing += wanted.iter().filter(|i| !library.is_imported(i)).count();
        report.folders_done += 1;
        state.folders_done += 1;
        progress(&state);
    }
    Ok(report)
}

fn list(
    source: &mut dyn PhoneSource,
    folder: &PhoneFolder,
    cancel: &AtomicBool,
    retry_pause: Duration,
) -> Result<crate::library::Listing> {
    let mut last = None;
    for attempt in 0..LIST_ATTEMPTS {
        if attempt > 0 {
            if cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            std::thread::sleep(retry_pause);
        }
        match source.list(folder) {
            Ok(l) => return Ok(l),
            Err(e) if is_session_lost(&e) => return Err(e),
            Err(e) => {
                log::debug!("listing {} attempt {}: {e:#}", folder.name, attempt + 1);
                last = Some(e);
            }
        }
    }
    Err(last.expect("at least one attempt"))
}

/// Copies `files` that aren't on the PC yet. Returns true if the phone dropped
/// the connection. A file that fails with a "lost connection" error while the
/// phone still lists `folder` is skipped (and left missing) instead.
#[allow(clippy::too_many_arguments)]
fn copy_folder(
    library: &mut Library,
    source: &mut dyn PhoneSource,
    folder: &PhoneFolder,
    files: &[PhoneItem],
    cancel: &AtomicBool,
    report: &mut WalkReport,
    state: &mut WalkProgress,
    progress: &mut dyn FnMut(&WalkProgress),
) -> Result<bool> {
    let mut skip: HashSet<String> = HashSet::new();
    let mut idle = 0;
    while idle < FOLDER_PASSES {
        let todo: Vec<&PhoneItem> = files
            .iter()
            .filter(|i| !skip.contains(&i.id) && !library.is_imported(i))
            .collect();
        if todo.is_empty() {
            break;
        }
        let (base_copied, base_dup) = (state.copied, state.duplicates);
        let result = transfer::run_pass(library, source, &todo, cancel, &mut |p| {
            state.copied = base_copied + p.copied;
            state.duplicates = base_dup + p.duplicates;
            progress(state);
        })?;
        report.copied += result.copied;
        report.duplicates += result.duplicates;
        state.copied = base_copied + result.copied;
        state.duplicates = base_dup + result.duplicates;
        if result.session_lost {
            let still_there = source.list(folder).is_ok();
            match result.lost_on {
                Some(id) if still_there && skip.len() < MAX_SKIPS => {
                    log::info!("the phone still answers; skipping the file that failed");
                    skip.insert(id);
                    continue;
                }
                _ => return Ok(true),
            }
        }
        if result.progress() == 0 {
            idle += 1;
        } else {
            idle = 0;
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::io::{Cursor, Read};
    use std::path::PathBuf;

    use crate::library::{Listing, SessionLost, month_from_folder};

    fn heic(seed: u8, len: usize) -> Vec<u8> {
        let mut v = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic".to_vec();
        v.resize(len, seed);
        v
    }

    /// A phone with a folder tree: root > Internal Storage > month folders.
    #[derive(Default)]
    struct TreePhone {
        folders: HashMap<String, Vec<String>>,
        files: HashMap<String, Vec<(PhoneItem, Vec<u8>)>>,
        /// Folder name -> listing failures left before it lists.
        flaky: HashMap<String, usize>,
        /// Folder whose listing reports a lost connection.
        drops_at: Option<String>,
        /// File whose download reports a lost connection, while listing works.
        bad_file: Option<String>,
        lists: usize,
    }

    impl TreePhone {
        fn new(months: &[(&str, usize)]) -> Self {
            let mut p = Self::default();
            p.folders
                .insert("root".into(), vec!["Internal Storage".into()]);
            p.folders.insert(
                "Internal Storage".into(),
                months.iter().map(|(m, _)| m.to_string()).collect(),
            );
            let mut seed = 0u8;
            for (month, count) in months {
                let files = (0..*count)
                    .map(|i| {
                        seed = seed.wrapping_add(1);
                        let data = heic(seed, 100 + seed as usize);
                        let item = PhoneItem {
                            id: format!("{month}/{i}"),
                            name: format!("IMG_{seed:04}.HEIC"),
                            size: data.len() as u64,
                            month: month_from_folder(month),
                            video: false,
                        };
                        (item, data)
                    })
                    .collect();
                p.files.insert(month.to_string(), files);
            }
            p
        }

        fn all_files(&self) -> Vec<&(PhoneItem, Vec<u8>)> {
            self.files.values().flatten().collect()
        }
    }

    impl PhoneSource for TreePhone {
        fn root(&mut self) -> Result<PhoneFolder> {
            Ok(PhoneFolder {
                id: "root".into(),
                name: "root".into(),
            })
        }

        fn list(&mut self, folder: &PhoneFolder) -> Result<Listing> {
            self.lists += 1;
            if self.drops_at.as_deref() == Some(folder.id.as_str()) {
                return Err(SessionLost("test".into()).into());
            }
            if let Some(left) = self.flaky.get_mut(&folder.id)
                && *left > 0
            {
                *left -= 1;
                anyhow::bail!("element not found");
            }
            let folders = self
                .folders
                .get(&folder.id)
                .into_iter()
                .flatten()
                .map(|f| PhoneFolder {
                    id: f.clone(),
                    name: f.clone(),
                })
                .collect();
            let files = self
                .files
                .get(&folder.id)
                .into_iter()
                .flatten()
                .map(|(i, _)| i.clone())
                .collect();
            Ok(Listing {
                folders,
                files,
                unreadable: 0,
            })
        }

        fn open(&mut self, item: &PhoneItem) -> Result<Box<dyn Read + '_>> {
            if self.bad_file.as_deref() == Some(item.name.as_str()) {
                return Err(SessionLost("proxy registration".into()).into());
            }
            let data = self
                .all_files()
                .into_iter()
                .find(|(i, _)| i.id == item.id)
                .unwrap()
                .1
                .clone();
            Ok(Box::new(Cursor::new(data)))
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

    fn walk(env: &Env, phone: &mut TreePhone) -> (WalkReport, Vec<WalkProgress>) {
        let mut lib = Library::open(&env.dest(), &env.state).unwrap();
        let mut seen = Vec::new();
        let report = walk_and_copy(
            &mut lib,
            phone,
            &AtomicBool::new(false),
            Duration::ZERO,
            &mut |p| seen.push(p.clone()),
        )
        .unwrap();
        lib.finish().unwrap();
        (report, seen)
    }

    fn assert_all_stored(env: &Env, phone: &TreePhone) {
        for (item, data) in phone.all_files() {
            let (y, m) = item.month.unwrap();
            let path = env
                .root
                .join(format!("{y:04}"))
                .join(format!("{m:02}"))
                .join(&item.name);
            assert_eq!(&fs::read(&path).unwrap(), data, "{}", item.name);
        }
    }

    #[test]
    fn copies_every_folder_into_its_month() {
        let env = env();
        let mut phone = TreePhone::new(&[("202403_a", 3), ("202310_b", 2), ("201709_a", 1)]);
        let (report, progress) = walk(&env, &mut phone);
        assert_eq!(report.copied, 6);
        assert!(report.complete(), "{report:?}");
        assert_eq!(report.folders_done, 5); // root, Internal Storage, 3 months
        assert_all_stored(&env, &phone);
        // Progress reports a growing number of known folders.
        assert_eq!(progress.last().unwrap().folders_known, 5);
        assert!(progress.iter().any(|p| p.folders_known == 2));
    }

    #[test]
    fn second_walk_copies_nothing() {
        let env = env();
        let mut phone = TreePhone::new(&[("202403_a", 3)]);
        walk(&env, &mut phone);
        let (report, _) = walk(&env, &mut phone);
        assert_eq!((report.copied, report.missing), (0, 0));
        assert!(report.complete());
    }

    #[test]
    fn flaky_folder_is_retried() {
        let env = env();
        let mut phone = TreePhone::new(&[("202403_a", 2), ("202404_a", 2)]);
        phone.flaky.insert("202403_a".into(), 2);
        let (report, _) = walk(&env, &mut phone);
        assert_eq!(report.copied, 4);
        assert!(report.complete());
    }

    #[test]
    fn folder_that_never_lists_is_reported() {
        let env = env();
        let mut phone = TreePhone::new(&[("202403_a", 2), ("202404_a", 2)]);
        phone.flaky.insert("202403_a".into(), 99);
        let (report, _) = walk(&env, &mut phone);
        assert_eq!((report.copied, report.folders_failed), (2, 1));
        assert!(!report.complete());
    }

    #[test]
    fn lost_connection_stops_and_the_next_walk_resumes() {
        let env = env();
        let mut phone = TreePhone::new(&[("202403_a", 2), ("202404_a", 2)]);
        phone.drops_at = Some("202404_a".into());
        let (first, _) = walk(&env, &mut phone);
        assert!(first.session_lost);
        assert_eq!(first.copied, 2);
        assert!(!first.complete());

        phone.drops_at = None;
        let (second, _) = walk(&env, &mut phone);
        assert_eq!(second.copied, 2);
        assert!(second.complete());
        assert_all_stored(&env, &phone);
    }

    #[test]
    fn one_failing_file_does_not_stop_the_walk() {
        let env = env();
        let mut phone = TreePhone::new(&[("202403_a", 3), ("202404_a", 2)]);
        let bad = phone.files["202403_a"][1].0.name.clone();
        phone.bad_file = Some(bad);
        let (report, _) = walk(&env, &mut phone);
        assert_eq!(report.copied, 4);
        assert_eq!(report.missing, 1);
        assert!(!report.session_lost);
        assert_eq!(report.folders_done, 4);
    }

    #[test]
    fn videos_are_skipped_without_a_videos_folder() {
        let env = env();
        let mut phone = TreePhone::new(&[("202403_a", 2)]);
        let mut clip = b"\0\0\0\x14ftypqt  \0\0\0\0qt  ".to_vec();
        clip.resize(300, 9);
        let item = PhoneItem {
            id: "202403_a/v".into(),
            name: "IMG_9999.MOV".into(),
            size: clip.len() as u64,
            month: Some((2024, 3)),
            video: false,
        };
        phone.files.get_mut("202403_a").unwrap().push((item, clip));
        let (report, _) = walk(&env, &mut phone);
        assert_eq!(
            (report.copied, report.skipped_videos, report.missing),
            (2, 1, 0)
        );
        assert!(report.complete());
        assert!(
            !env.root
                .join("2024")
                .join("03")
                .join("IMG_9999.MOV")
                .exists()
        );
    }
}
