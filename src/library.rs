//! The destination folder and the rules for adding to it.
//!
//! Safety rules, in order of importance:
//! - Nothing in the destination is ever overwritten or deleted. New files are
//!   created with no-clobber primitives (`hard_link` / `create_new`), and a name
//!   that is taken gets a ` (1)`, ` (2)`, … suffix instead.
//! - A photo whose bytes already exist anywhere in the destination is not stored
//!   again, whatever its name or folder. Content is compared by SHA-256.
//! - The import record (`library.json`) only lets us skip downloads; losing it
//!   costs time, never correctness.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Hidden folder inside the destination where downloads land before they are
/// linked into place. Kept on the same volume so the final step is a hard link.
pub const TEMP_DIR: &str = ".iphone-photo-sync-tmp";
const PARTIAL_EXT: &str = "partial";
const SAVE_EVERY: usize = 20;
const HEAD_LEN: usize = 32;

/// A file on the phone.
#[derive(Debug, Clone)]
pub struct PhoneItem {
    /// The source's handle for the file, only meaningful to that source.
    pub id: String,
    pub name: String,
    pub size: u64,
    /// Year and month from the phone folder the file is in (iOS names them
    /// like `202403_a`); picks the `YYYY/MM` folder on the PC.
    pub month: Option<(u16, u8)>,
    /// A standalone video (see [`mark_videos`]); goes to the videos folder.
    pub video: bool,
}

impl PhoneItem {
    /// Identifies the photo across connections: the phone's own handles are
    /// not stable, but name + size + month are.
    fn key(&self) -> String {
        let month = self
            .month
            .map(|(y, m)| format!("{y:04}{m:02}"))
            .unwrap_or_default();
        format!("{}|{}|{month}", self.name, self.size)
    }

    fn folder(&self) -> PathBuf {
        match self.month {
            Some((y, m)) => PathBuf::from(format!("{y:04}")).join(format!("{m:02}")),
            None => PathBuf::from("Unknown date"),
        }
    }
}

const VIDEO_EXTENSIONS: [&str; 4] = ["mov", "mp4", "m4v", "3gp"];

fn split_ext(name: &str) -> (&str, String) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, ext.to_ascii_lowercase()),
        _ => (name, String::new()),
    }
}

/// Marks the standalone videos among one phone folder's files. The short clip
/// of a Live Photo (`IMG_0333.MOV` next to `IMG_0333.HEIC`) is part of the
/// photo, so it stays with the photos.
pub fn mark_videos(files: &mut [PhoneItem]) {
    let photo_stems: HashSet<String> = files
        .iter()
        .filter(|f| !VIDEO_EXTENSIONS.contains(&split_ext(&f.name).1.as_str()))
        .map(|f| split_ext(&f.name).0.to_ascii_lowercase())
        .collect();
    for f in files {
        let (stem, ext) = split_ext(&f.name);
        f.video = VIDEO_EXTENSIONS.contains(&ext.as_str())
            && !photo_stems.contains(&stem.to_ascii_lowercase());
    }
}

/// Where things go on the PC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destinations {
    pub photos: PathBuf,
    /// `None`: videos are skipped.
    pub videos: Option<PathBuf>,
}

/// Reads the year and month from an iOS photo folder name such as `202403_a`.
pub fn month_from_folder(name: &str) -> Option<(u16, u8)> {
    let digits = name.get(..6)?;
    if !digits.bytes().all(|b| b.is_ascii_digit())
        || name[6..].starts_with(|c: char| c.is_ascii_digit())
    {
        return None;
    }
    let year: u16 = digits[..4].parse().ok()?;
    let month: u8 = digits[4..].parse().ok()?;
    ((1990..=2200).contains(&year) && (1..=12).contains(&month)).then_some((year, month))
}

/// A folder on the phone.
#[derive(Debug, Clone)]
pub struct PhoneFolder {
    /// The source's handle for the folder.
    pub id: String,
    pub name: String,
}

/// What one folder contains.
#[derive(Debug, Default)]
pub struct Listing {
    pub folders: Vec<PhoneFolder>,
    pub files: Vec<PhoneItem>,
    /// Objects whose details couldn't be read (often photos kept only in iCloud).
    pub unreadable: usize,
}

/// Where photos come from: a connected phone, or a fake one in tests. The
/// phone is read one folder at a time.
pub trait PhoneSource {
    fn root(&mut self) -> Result<PhoneFolder>;
    fn list(&mut self, folder: &PhoneFolder) -> Result<Listing>;
    fn open(&mut self, item: &PhoneItem) -> Result<Box<dyn Read + '_>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Imported on an earlier run and still in the destination; not downloaded.
    #[cfg(test)]
    AlreadyImported,
    /// New: stored at this path, relative to the destination.
    Copied(PathBuf),
    /// Downloaded, but identical bytes already exist at this relative path.
    Duplicate(PathBuf),
}

/// A phone error that means its session is gone ("device not functioning",
/// "shutdown was already called"). The phone has to be reopened.
#[derive(Debug)]
pub struct SessionLost(pub String);

impl std::fmt::Display for SessionLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lost the connection to the phone: {}", self.0)
    }
}

impl std::error::Error for SessionLost {}

/// Whether `e` (or an I/O error inside it) is a [`SessionLost`].
pub fn is_session_lost(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.is::<SessionLost>()
            || c.downcast_ref::<io::Error>()
                .and_then(|io| io.get_ref())
                .is_some_and(|inner| inner.is::<SessionLost>())
    })
}

/// Returned (inside `anyhow::Error`) when an import is cancelled.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Bytes received from the phone, waiting in the temp folder.
#[derive(Debug)]
pub struct Download {
    temp: PathBuf,
    pub size: u64,
    pub sha256: String,
    /// The first bytes, for recognising the file format.
    pub head: Vec<u8>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// `PhoneItem::key` → where that file is (or was) on the PC.
    imported: HashMap<String, PathBuf>,
    /// Hashes of destination files, valid while size and mtime match.
    hashes: HashMap<PathBuf, Hashed>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Hashed {
    size: u64,
    modified: u128,
    sha256: String,
}

pub struct Library {
    dest: Destinations,
    state_path: PathBuf,
    state: State,
    /// Every file in the destination folders (absolute paths), by size: only
    /// same-size files can be duplicates, so most downloads never need an
    /// existing file hashed.
    by_size: HashMap<u64, Vec<PathBuf>>,
    unsaved: usize,
    temp_seq: u64,
}

impl Library {
    /// Opens the destination folders, creating them if needed, with the import
    /// state kept at `state_path`. Scans them so duplicates can be found.
    pub fn open(dest: &Destinations, state_path: &Path) -> Result<Self> {
        for root in dest.roots() {
            fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        }
        let state = fs::read(state_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<State>(&b).ok())
            .unwrap_or_default();

        let mut lib = Self {
            dest: dest.clone(),
            state_path: state_path.to_owned(),
            state,
            by_size: HashMap::new(),
            unsaved: 0,
            temp_seq: 0,
        };
        lib.clean_temp();
        lib.scan()?;
        Ok(lib)
    }

    /// Whether `item` can be skipped without downloading it: recorded as
    /// imported, and that file is still in one of the current folders.
    pub fn is_imported(&self, item: &PhoneItem) -> bool {
        self.state
            .imported
            .get(&item.key())
            .is_some_and(|path| self.dest.contains(path) && path.is_file())
    }

    /// Whether this sync copies `item` at all (videos need a videos folder).
    pub fn wants(&self, item: &PhoneItem) -> bool {
        !item.video || self.dest.videos.is_some()
    }

    fn root_for(&self, item: &PhoneItem) -> &Path {
        match (&self.dest.videos, item.video) {
            (Some(videos), true) => videos,
            _ => &self.dest.photos,
        }
    }

    /// Streams whatever the phone sends for a request for `item` into a temp
    /// file. The data is *not* trusted to be `item`: see [`crate::transfer`].
    pub fn download(
        &mut self,
        source: &mut dyn PhoneSource,
        item: &PhoneItem,
        cancel: &AtomicBool,
    ) -> Result<Download> {
        let root = self.root_for(item).to_path_buf();
        let temp = self.temp_path(&root)?;
        match download(source, item, &temp, cancel) {
            Ok((size, sha256, head)) => Ok(Download {
                temp,
                size,
                sha256,
                head,
            }),
            Err(e) => {
                let _ = fs::remove_file(&temp);
                Err(e)
            }
        }
    }

    /// Throws a download away.
    pub fn discard(&self, download: Download) {
        let _ = fs::remove_file(download.temp);
    }

    /// Adds a download to the destination as `item`, unless identical bytes are
    /// already there.
    pub fn store(&mut self, item: &PhoneItem, download: Download) -> Result<Outcome> {
        let Download {
            temp, size, sha256, ..
        } = download;
        if let Some(existing) = self.find_duplicate(size, &sha256) {
            fs::remove_file(&temp)?;
            self.record(item, existing.clone());
            return Ok(Outcome::Duplicate(self.dest.relative(&existing)));
        }

        let root = self.root_for(item).to_owned();
        let folder = item.folder();
        fs::create_dir_all(root.join(&folder))?;
        let rel = match place(&temp, &root, &folder, &sanitize(&item.name)) {
            Ok(rel) => rel,
            Err(e) => {
                let _ = fs::remove_file(&temp);
                return Err(e);
            }
        };
        let path = root.join(&rel);
        let meta = fs::metadata(&path)?;
        self.state.hashes.insert(
            path.clone(),
            Hashed {
                size,
                modified: mtime(&meta),
                sha256,
            },
        );
        self.by_size.entry(size).or_default().push(path.clone());
        self.record(item, path);
        Ok(Outcome::Copied(rel))
    }

    /// Download + store for a phone that is known to answer the right request.
    #[cfg(test)]
    pub fn import(
        &mut self,
        source: &mut dyn PhoneSource,
        item: &PhoneItem,
        cancel: &AtomicBool,
    ) -> Result<Outcome> {
        if self.is_imported(item) {
            return Ok(Outcome::AlreadyImported);
        }
        let download = self.download(source, item, cancel)?;
        self.store(item, download)
    }

    /// Writes the import state. Called periodically during an import and at the end.
    pub fn save(&mut self) -> Result<()> {
        if let Some(dir) = self.state_path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.state_path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec(&self.state)?)?;
        fs::rename(&tmp, &self.state_path)?;
        self.unsaved = 0;
        Ok(())
    }

    /// Saves the state and removes the temp folders if they are empty.
    pub fn finish(&mut self) -> Result<()> {
        for root in self.dest.roots() {
            let _ = fs::remove_dir(root.join(TEMP_DIR));
        }
        self.save()
    }

    fn record(&mut self, item: &PhoneItem, rel: PathBuf) {
        self.state.imported.insert(item.key(), rel);
        self.unsaved += 1;
        if self.unsaved >= SAVE_EVERY
            && let Err(e) = self.save()
        {
            log::warn!("saving import state: {e:#}");
        }
    }

    fn find_duplicate(&mut self, size: u64, sha256: &str) -> Option<PathBuf> {
        let candidates = self.by_size.get(&size)?.clone();
        candidates
            .into_iter()
            .find(|path| match self.hash_existing(path) {
                Ok(h) => h == sha256,
                Err(e) => {
                    log::warn!("hashing {}: {e:#}", path.display());
                    false
                }
            })
    }

    fn hash_existing(&mut self, path: &Path) -> Result<String> {
        let meta = fs::metadata(path)?;
        let modified = mtime(&meta);
        if let Some(h) = self.state.hashes.get(path)
            && h.size == meta.len()
            && h.modified == modified
        {
            return Ok(h.sha256.clone());
        }
        let sha256 = hash_file(path)?;
        self.state.hashes.insert(
            path.to_owned(),
            Hashed {
                size: meta.len(),
                modified,
                sha256: sha256.clone(),
            },
        );
        Ok(sha256)
    }

    fn temp_path(&mut self, root: &Path) -> Result<PathBuf> {
        let dir = root.join(TEMP_DIR);
        if !dir.is_dir() {
            fs::create_dir_all(&dir)?;
            hide(&dir);
        }
        self.temp_seq += 1;
        Ok(dir.join(format!(
            "{}-{}.{PARTIAL_EXT}",
            std::process::id(),
            self.temp_seq
        )))
    }

    /// Removes leftovers of interrupted downloads: only our own `*.partial`
    /// files inside our own temp folder.
    fn clean_temp(&self) {
        for root in self.dest.roots() {
            let Ok(entries) = fs::read_dir(root.join(TEMP_DIR)) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == PARTIAL_EXT) && path.is_file() {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }

    fn scan(&mut self) -> Result<()> {
        // The folders may be the same or nested; index each file once.
        let mut seen = HashSet::new();
        for root in self.dest.roots() {
            let mut stack = vec![root.to_owned()];
            while let Some(dir) = stack.pop() {
                let entries = match fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(e) if dir == root => return Err(e.into()),
                    Err(e) => {
                        log::warn!("skipping {}: {e}", dir.display());
                        continue;
                    }
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Ok(ft) = entry.file_type() else { continue };
                    if ft.is_dir() {
                        if entry.file_name() != TEMP_DIR {
                            stack.push(path);
                        }
                    } else if ft.is_file()
                        && let Ok(meta) = entry.metadata()
                        && seen.insert(path.clone())
                    {
                        self.by_size.entry(meta.len()).or_default().push(path);
                    }
                }
            }
        }
        Ok(())
    }
}

impl Destinations {
    fn roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.photos.as_path()).chain(self.videos.as_deref())
    }

    fn contains(&self, path: &Path) -> bool {
        self.roots().any(|r| path.starts_with(r))
    }

    /// `path` relative to the folder it is in, for messages.
    fn relative(&self, path: &Path) -> PathBuf {
        self.roots()
            .filter_map(|r| path.strip_prefix(r).ok())
            .min_by_key(|p| p.components().count())
            .map_or_else(|| path.to_owned(), Path::to_path_buf)
    }
}

/// Streams `item` into `temp`, returning its size, SHA-256 and first bytes.
fn download(
    source: &mut dyn PhoneSource,
    item: &PhoneItem,
    temp: &Path,
    cancel: &AtomicBool,
) -> Result<(u64, String, Vec<u8>)> {
    let mut out = File::create_new(temp)?;
    let mut reader = source.open(item)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    let mut head = Vec::with_capacity(HEAD_LEN);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("reading from the phone"),
        };
        if head.len() < HEAD_LEN {
            let take = (HEAD_LEN - head.len()).min(n);
            head.extend_from_slice(&buf[..take]);
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        size += n as u64;
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    out.sync_all()?;
    Ok((size, hex::encode(hasher.finalize()), head))
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match file.read(&mut buf) {
            Ok(0) => return Ok(hex::encode(hasher.finalize())),
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Moves `temp` to `root/folder/name` without ever replacing an existing file:
/// a taken name becomes `name (1).ext`, `name (2).ext`, …
fn place(temp: &Path, root: &Path, folder: &Path, name: &str) -> Result<PathBuf> {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    for n in 0..10_000u32 {
        let file = if n == 0 {
            name.to_owned()
        } else {
            format!("{stem} ({n}){ext}")
        };
        let rel = folder.join(file);
        let dest = root.join(&rel);
        // A hard link fails if `dest` exists, so it cannot clobber anything.
        match fs::hard_link(temp, &dest) {
            Ok(()) => {
                fs::remove_file(temp)?;
                return Ok(rel);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => {}
        }
        // Filesystems without hard links (exFAT, some network shares): copy into
        // a file that we create exclusively.
        let mut out = match OpenOptions::new().write(true).create_new(true).open(&dest) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", dest.display())),
        };
        let copied = io::copy(&mut File::open(temp)?, &mut out).and_then(|_| out.sync_all());
        if let Err(e) = copied {
            drop(out);
            // Only ever the file we just created ourselves.
            let _ = fs::remove_file(&dest);
            return Err(e).with_context(|| format!("writing {}", dest.display()));
        }
        fs::remove_file(temp)?;
        return Ok(rel);
    }
    bail!("no free file name for {name} in {}", folder.display())
}

/// Makes a phone file name safe to use on Windows.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').to_owned();
    if cleaned.is_empty() {
        "unnamed".to_owned()
    } else {
        cleaned
    }
}

fn mtime(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

#[cfg(windows)]
fn hide(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};
    use windows::core::PCWSTR;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call.
    let _ = unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_HIDDEN) };
}

#[cfg(not(windows))]
fn hide(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[derive(Default)]
    struct FakePhone {
        files: Vec<(PhoneItem, Vec<u8>)>,
        opened: usize,
    }

    impl FakePhone {
        fn add(&mut self, name: &str, month: Option<(u16, u8)>, data: &[u8]) -> PhoneItem {
            let item = PhoneItem {
                id: format!("o{}", self.files.len()),
                name: name.into(),
                size: data.len() as u64,
                month,
                video: false,
            };
            self.files.push((item.clone(), data.to_vec()));
            item
        }
    }

    impl FakePhone {
        fn items(&self) -> Vec<PhoneItem> {
            self.files.iter().map(|(i, _)| i.clone()).collect()
        }
    }

    impl PhoneSource for FakePhone {
        fn root(&mut self) -> Result<PhoneFolder> {
            Ok(PhoneFolder {
                id: "root".into(),
                name: "root".into(),
            })
        }

        fn list(&mut self, _folder: &PhoneFolder) -> Result<Listing> {
            Ok(Listing {
                files: self.items(),
                ..Listing::default()
            })
        }

        fn open(&mut self, item: &PhoneItem) -> Result<Box<dyn Read + '_>> {
            self.opened += 1;
            let data = &self.files.iter().find(|(i, _)| i.id == item.id).unwrap().1;
            Ok(Box::new(Cursor::new(data.clone())))
        }
    }

    const MAR_2024: (u16, u8) = (2024, 3);

    struct Env {
        _dir: tempfile::TempDir,
        root: PathBuf,
        state: PathBuf,
    }

    impl Env {
        fn dest(&self) -> Destinations {
            Destinations {
                photos: self.root.clone(),
                videos: None,
            }
        }
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Photos");
        let state = dir.path().join("state").join("library.json");
        Env {
            _dir: dir,
            root,
            state,
        }
    }

    fn import_all(lib: &mut Library, phone: &mut FakePhone) -> Vec<Outcome> {
        let cancel = AtomicBool::new(false);
        let items = phone.items();
        let out = items
            .iter()
            .map(|i| lib.import(phone, i, &cancel).unwrap())
            .collect();
        lib.finish().unwrap();
        out
    }

    #[test]
    fn copies_into_year_month_folders() {
        let env = env();
        let mut phone = FakePhone::default();
        phone.add("IMG_0001.HEIC", Some(MAR_2024), b"photo one");
        phone.add("IMG_0002.MOV", None, b"video");
        let mut lib = Library::open(&env.dest(), &env.state).unwrap();

        let out = import_all(&mut lib, &mut phone);
        let a = Path::new("2024").join("03").join("IMG_0001.HEIC");
        let b = Path::new("Unknown date").join("IMG_0002.MOV");
        assert_eq!(
            out,
            [Outcome::Copied(a.clone()), Outcome::Copied(b.clone())]
        );
        assert_eq!(fs::read(env.root.join(a)).unwrap(), b"photo one");
        assert_eq!(fs::read(env.root.join(b)).unwrap(), b"video");
        assert!(!env.root.join(TEMP_DIR).exists());
    }

    #[test]
    fn second_run_skips_without_downloading() {
        let env = env();
        let mut phone = FakePhone::default();
        phone.add("IMG_0001.HEIC", Some(MAR_2024), b"photo one");
        import_all(
            &mut Library::open(&env.dest(), &env.state).unwrap(),
            &mut phone,
        );
        phone.opened = 0;

        let mut lib = Library::open(&env.dest(), &env.state).unwrap();
        assert_eq!(import_all(&mut lib, &mut phone), [Outcome::AlreadyImported]);
        assert_eq!(phone.opened, 0);
    }

    #[test]
    fn never_overwrites_a_different_file_with_the_same_name() {
        let env = env();
        let folder = env.root.join("2024").join("03");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("IMG_0001.HEIC"), b"someone else's photo").unwrap();
        fs::write(folder.join("IMG_0001 (1).HEIC"), b"and another").unwrap();

        let mut phone = FakePhone::default();
        phone.add("IMG_0001.HEIC", Some(MAR_2024), b"photo one");
        let mut lib = Library::open(&env.dest(), &env.state).unwrap();
        let out = import_all(&mut lib, &mut phone);

        let rel = Path::new("2024").join("03").join("IMG_0001 (2).HEIC");
        assert_eq!(out, [Outcome::Copied(rel.clone())]);
        assert_eq!(
            fs::read(folder.join("IMG_0001.HEIC")).unwrap(),
            b"someone else's photo"
        );
        assert_eq!(
            fs::read(folder.join("IMG_0001 (1).HEIC")).unwrap(),
            b"and another"
        );
        assert_eq!(fs::read(env.root.join(rel)).unwrap(), b"photo one");
    }

    #[test]
    fn existing_content_anywhere_is_not_stored_twice() {
        let env = env();
        let other = env.root.join("Old backup");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("renamed.heic"), b"photo one").unwrap();
        fs::write(other.join("same-size.heic"), b"photo two").unwrap();

        let mut phone = FakePhone::default();
        phone.add("IMG_0001.HEIC", Some(MAR_2024), b"photo one");
        let mut lib = Library::open(&env.dest(), &env.state).unwrap();
        let out = import_all(&mut lib, &mut phone);

        let existing = Path::new("Old backup").join("renamed.heic");
        assert_eq!(out, [Outcome::Duplicate(existing)]);
        assert!(!env.root.join("2024").exists());

        // Recorded, so the next run doesn't download it again.
        phone.opened = 0;
        let mut lib = Library::open(&env.dest(), &env.state).unwrap();
        assert_eq!(import_all(&mut lib, &mut phone), [Outcome::AlreadyImported]);
        assert_eq!(phone.opened, 0);
    }

    #[test]
    fn identical_files_on_the_phone_are_stored_once() {
        let env = env();
        let mut phone = FakePhone::default();
        phone.add("IMG_0001.JPG", Some(MAR_2024), b"same bytes");
        phone.add("IMG_0001 copy.JPG", Some(MAR_2024), b"same bytes");
        let mut lib = Library::open(&env.dest(), &env.state).unwrap();

        let rel = Path::new("2024").join("03").join("IMG_0001.JPG");
        assert_eq!(
            import_all(&mut lib, &mut phone),
            [Outcome::Copied(rel.clone()), Outcome::Duplicate(rel)]
        );
    }

    #[test]
    fn photo_removed_from_pc_is_added_again() {
        let env = env();
        let mut phone = FakePhone::default();
        phone.add("IMG_0001.HEIC", Some(MAR_2024), b"photo one");
        import_all(
            &mut Library::open(&env.dest(), &env.state).unwrap(),
            &mut phone,
        );
        let path = env.root.join("2024").join("03").join("IMG_0001.HEIC");
        fs::remove_file(&path).unwrap();

        let mut lib = Library::open(&env.dest(), &env.state).unwrap();
        let out = import_all(&mut lib, &mut phone);
        assert!(matches!(out[0], Outcome::Copied(_)));
        assert!(path.is_file());
    }

    #[test]
    fn lost_state_still_dedups_by_content() {
        let env = env();
        let mut phone = FakePhone::default();
        phone.add("IMG_0001.HEIC", Some(MAR_2024), b"photo one");
        import_all(
            &mut Library::open(&env.dest(), &env.state).unwrap(),
            &mut phone,
        );
        fs::remove_file(&env.state).unwrap();

        let mut lib = Library::open(&env.dest(), &env.state).unwrap();
        let rel = Path::new("2024").join("03").join("IMG_0001.HEIC");
        assert_eq!(import_all(&mut lib, &mut phone), [Outcome::Duplicate(rel)]);
        let files: Vec<_> = fs::read_dir(env.root.join("2024").join("03"))
            .unwrap()
            .collect();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn cancel_leaves_no_partial_files() {
        let env = env();
        let mut phone = FakePhone::default();
        let item = phone.add("IMG_0001.HEIC", Some(MAR_2024), b"photo one");
        let mut lib = Library::open(&env.dest(), &env.state).unwrap();

        let err = lib
            .import(&mut phone, &item, &AtomicBool::new(true))
            .unwrap_err();
        assert!(err.is::<Cancelled>());
        let temp_files = fs::read_dir(env.root.join(TEMP_DIR)).unwrap().count();
        assert_eq!(temp_files, 0);
        assert!(!env.root.join("2024").exists());
    }

    #[test]
    fn stale_partials_are_cleaned_but_other_files_kept() {
        let env = env();
        let temp = env.root.join(TEMP_DIR);
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("1-1.partial"), b"junk").unwrap();
        fs::write(temp.join("keep.txt"), b"not ours").unwrap();

        Library::open(&env.dest(), &env.state).unwrap();
        assert!(!temp.join("1-1.partial").exists());
        assert!(temp.join("keep.txt").exists());
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize("IMG_0001.HEIC"), "IMG_0001.HEIC");
        assert_eq!(sanitize("a:b/c?.jpg"), "a_b_c_.jpg");
        assert_eq!(sanitize(" ..."), "unnamed");
    }

    #[test]
    fn standalone_videos_are_told_apart_from_live_photo_clips() {
        let item = |name: &str| PhoneItem {
            id: name.into(),
            name: name.into(),
            size: 1,
            month: None,
            video: false,
        };
        let mut files = vec![
            item("IMG_0333.HEIC"),
            item("IMG_0333.MOV"),
            item("IMG_0334.MOV"),
            item("IMG_0335.mp4"),
            item("IMG_0336.JPG"),
        ];
        mark_videos(&mut files);
        let videos: Vec<_> = files
            .iter()
            .filter(|f| f.video)
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(videos, ["IMG_0334.MOV", "IMG_0335.mp4"]);
    }

    #[test]
    fn videos_go_to_their_own_folder_and_dedup_spans_both() {
        let env = env();
        let videos = env.root.parent().unwrap().join("Videos");
        let dest = Destinations {
            photos: env.root.clone(),
            videos: Some(videos.clone()),
        };
        let mut phone = FakePhone::default();
        phone.add("IMG_0001.HEIC", Some(MAR_2024), b"photo");
        let mut clip = phone.add("IMG_0002.MOV", Some(MAR_2024), b"a video");
        phone.files[1].0.video = true;
        clip.video = true;
        let mut lib = Library::open(&dest, &env.state).unwrap();
        let cancel = AtomicBool::new(false);
        for (item, _) in phone.files.clone() {
            lib.import(&mut phone, &item, &cancel).unwrap();
        }
        lib.finish().unwrap();
        assert!(
            env.root
                .join("2024")
                .join("03")
                .join("IMG_0001.HEIC")
                .is_file()
        );
        assert!(
            videos
                .join("2024")
                .join("03")
                .join("IMG_0002.MOV")
                .is_file()
        );
        assert!(
            !env.root
                .join("2024")
                .join("03")
                .join("IMG_0002.MOV")
                .exists()
        );

        // Without a videos folder the video is not wanted, but stays imported
        // once a videos folder is set again.
        let lib = Library::open(&env.dest(), &env.state).unwrap();
        assert!(!lib.wants(&clip));
        let lib = Library::open(&dest, &env.state).unwrap();
        assert!(lib.wants(&clip) && lib.is_imported(&clip));
    }

    #[test]
    fn changing_the_photos_folder_copies_again() {
        let env = env();
        let mut phone = FakePhone::default();
        let item = phone.add("IMG_0001.HEIC", Some(MAR_2024), b"photo one");
        import_all(
            &mut Library::open(&env.dest(), &env.state).unwrap(),
            &mut phone,
        );
        let other = Destinations {
            photos: env.root.parent().unwrap().join("Elsewhere"),
            videos: None,
        };
        let lib = Library::open(&other, &env.state).unwrap();
        assert!(!lib.is_imported(&item));
    }

    #[test]
    fn reads_months_from_folder_names() {
        assert_eq!(month_from_folder("202403_a"), Some((2024, 3)));
        assert_eq!(month_from_folder("201709_b"), Some((2017, 9)));
        assert_eq!(month_from_folder("202413_a"), None);
        assert_eq!(month_from_folder("2024031_a"), None);
        assert_eq!(month_from_folder("DCIM"), None);
        assert_eq!(month_from_folder("Internal Storage"), None);
    }
}
