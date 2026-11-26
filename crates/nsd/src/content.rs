//! Request content and temp files.
//!
//! * [`get_temp`] / [`release_temp`] port `Ns_GetTemp` / `Ns_ReleaseTemp`
//!   (`nsd/fd.c`): a process-wide pool of open, already unlinked temp files
//!   used to spool large request bodies.
//! * [`Content`] is a request body as the driver read it ahead: in memory
//!   (`connPtr->content`) or in a temp file (`connPtr->tfd`,
//!   `NS_CONN_FILECONTENT`).  Whole-body access maps a spooled body on first
//!   use, as `NsConnContent` does; ranged reads go straight to the file.

use std::cell::OnceCell;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Free temp files (`firstTmpPtr`).
static TEMPS: Mutex<Vec<File>> = Mutex::new(Vec::new());

/// Where new temp files are created: `P_tmpdir`, a compile-time constant
/// in AOLserver (`TMPDIR` is not consulted).
pub const P_TMPDIR: &str = "/tmp";

/// A test override of [`P_TMPDIR`].
static TEMP_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Create new temp files in `dir` instead of [`P_TMPDIR`] (`None` restores
/// it).  A test hook with no AOLserver counterpart: it lets tests make
/// [`get_temp`] fail.
#[doc(hidden)]
pub fn set_temp_dir(dir: Option<&Path>) {
    *TEMP_DIR.write().unwrap_or_else(PoisonError::into_inner) = dir.map(Path::to_path_buf);
}

/// An open, unlinked temp file from the pool.  Dropping it returns the file
/// to the pool ([`release_temp`]).
#[derive(Debug)]
pub struct TempFile {
    file: Option<File>,
}

impl TempFile {
    /// The open file.
    pub fn file(&self) -> &File {
        self.file
            .as_ref()
            .expect("a TempFile holds its file until dropped")
    }

    /// The open file, mutably.
    pub fn file_mut(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("a TempFile holds its file until dropped")
    }

    /// Detach the file from the pool: it is closed rather than reused when
    /// dropped.
    pub fn into_file(mut self) -> File {
        self.file
            .take()
            .expect("a TempFile holds its file until dropped")
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(f) = self.file.take() {
            release_temp(f);
        }
    }
}

/// The directory new temp files are created in.
fn temp_dir() -> PathBuf {
    TEMP_DIR
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| PathBuf::from(P_TMPDIR))
}

/// Pop or create a temp file (`Ns_GetTemp`).  New files are created as
/// `nstmp.<sec>.<usec>` (mode 0600, exclusive) in [`P_TMPDIR`] and
/// unlinked at once, so their content cannot be snooped and
/// disappears with the last descriptor.
pub fn get_temp() -> io::Result<TempFile> {
    let pooled = TEMPS.lock().unwrap_or_else(PoisonError::into_inner).pop();
    if let Some(file) = pooled {
        return Ok(TempFile { file: Some(file) });
    }
    let dir = temp_dir();
    let mut trys = 0;
    loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let path = dir.join(format!("nstmp.{}.{}", now.as_secs(), now.subsec_micros()));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                // Unlinking can only fail if someone else removed it first.
                let _ = std::fs::remove_file(&path);
                return Ok(TempFile { file: Some(file) });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && trys < 10 => trys += 1,
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("could not open temp file {}: {e}", path.display()),
                ))
            }
        }
    }
}

/// Return a temp file to the pool (`Ns_ReleaseTemp`): it is rewound and
/// truncated, or closed if that fails.
pub fn release_temp(mut file: File) {
    if file.seek(SeekFrom::Start(0)).is_ok() && file.set_len(0).is_ok() {
        TEMPS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(file);
    }
}

/// Number of free temp files in the pool.
pub fn pooled_temps() -> usize {
    TEMPS.lock().unwrap_or_else(PoisonError::into_inner).len()
}

/// A request body (`connPtr->content`, `connPtr->tfd`).
#[derive(Default)]
pub struct Content {
    /// The body when it is in memory.
    mem: Option<Vec<u8>>,
    /// The temp file holding the body, when on disk.
    file: Option<TempFile>,
    /// `contentLength`.
    len: usize,
    /// The whole file-only body once mapped (`connPtr->map`).
    map: OnceCell<Vec<u8>>,
}

impl std::fmt::Debug for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Content")
            .field("len", &self.len)
            .field("in_memory", &self.mem.is_some())
            .field("on_disk", &self.file.is_some())
            .finish_non_exhaustive()
    }
}

impl From<Vec<u8>> for Content {
    fn from(v: Vec<u8>) -> Self {
        Self::memory(v)
    }
}

impl Content {
    /// A body held in memory.
    pub fn memory(data: Vec<u8>) -> Self {
        Self {
            len: data.len(),
            mem: Some(data),
            file: None,
            map: OnceCell::new(),
        }
    }

    /// A body of `len` bytes spooled to `file` (`NS_CONN_FILECONTENT`).
    pub fn spooled(file: TempFile, len: usize) -> Self {
        Self {
            mem: None,
            file: Some(file),
            len,
            map: OnceCell::new(),
        }
    }

    /// Body length (`Ns_ConnContentLength`).
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the body is empty.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the body is in a temp file (`Ns_ConnContentOnDisk`): spooled
    /// by the driver, or copied there by [`Content::fd`].
    pub const fn on_disk(&self) -> bool {
        self.file.is_some()
    }

    /// Whether the driver spooled the body (`NS_CONN_FILECONTENT`).
    pub const fn file_content(&self) -> bool {
        self.mem.is_none() && self.file.is_some()
    }

    /// The whole body (`Ns_ConnContent`), reading a spooled body into memory
    /// on first use as `NsConnContent` maps it.
    pub fn bytes(&self) -> io::Result<&[u8]> {
        if let Some(m) = &self.mem {
            return Ok(m);
        }
        if let Some(m) = self.map.get() {
            return Ok(m);
        }
        let data = self.read_file(0, self.len)?;
        Ok(self.map.get_or_init(|| data))
    }

    /// The whole body as a vector (empty if a spooled body cannot be read).
    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes().map(<[u8]>::to_vec).unwrap_or_default()
    }

    /// `len` bytes from `off`, clamped to the body.  A spooled body that has
    /// not been mapped is read from the file.
    pub fn read_at(&self, off: usize, len: usize) -> io::Result<Vec<u8>> {
        let start = off.min(self.len);
        let end = start.saturating_add(len).min(self.len);
        if let Some(m) = self.mem.as_ref().or_else(|| self.map.get()) {
            return Ok(m[start..end].to_vec());
        }
        self.read_file(start, end - start)
    }

    fn read_file(&self, off: usize, len: usize) -> io::Result<Vec<u8>> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no content file"))?;
        let mut buf = vec![0u8; len];
        file.file().read_exact_at(&mut buf, off as u64)?;
        Ok(buf)
    }

    /// The temp file holding the body (`Ns_ConnContentFd`), copying an
    /// in-memory body to a new temp file first.  The file is owned by the
    /// content; its offset is at the start of the body when it is created.
    pub fn fd(&mut self) -> io::Result<RawFd> {
        if let Some(f) = &self.file {
            return Ok(f.file().as_raw_fd());
        }
        let mut t = get_temp()?;
        if let Some(m) = &self.mem {
            if !m.is_empty() {
                t.file_mut().write_all(m)?;
                t.file_mut().seek(SeekFrom::Start(0))?;
            }
        }
        let fd = t.file().as_raw_fd();
        self.file = Some(t);
        Ok(fd)
    }

    /// A duplicate of [`Content::fd`]'s descriptor (`dup`), sharing its file
    /// offset, for handing the body to a child process as `nscgi` does.
    pub fn dup_fd(&mut self) -> io::Result<File> {
        self.fd()?;
        self.file
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no content file"))?
            .file()
            .try_clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn temp_files_are_unlinked_and_pooled() {
        let mut t = get_temp().expect("temp");
        t.file_mut().write_all(b"secret").expect("write");
        let fd = t.file().as_raw_fd();
        let link = std::fs::read_link(format!("/proc/self/fd/{fd}")).expect("link");
        assert!(link.to_string_lossy().ends_with("(deleted)"), "{link:?}");
        drop(t);
        assert!(pooled_temps() >= 1);
        // A reused file comes back empty.
        let mut t = get_temp().expect("temp");
        let mut s = String::new();
        t.file_mut().read_to_string(&mut s).expect("read");
        assert_eq!(s, "");
        assert_eq!(t.file().metadata().expect("meta").len(), 0);
        let f = t.into_file();
        drop(f);
    }

    #[test]
    fn memory_content() {
        let mut c = Content::memory(b"hello world".to_vec());
        assert_eq!((c.len(), c.is_empty(), c.on_disk()), (11, false, false));
        assert!(!c.file_content());
        assert_eq!(c.bytes().expect("bytes"), b"hello world");
        assert_eq!(c.read_at(6, 100).expect("range"), b"world");
        assert_eq!(c.read_at(50, 1).expect("range"), b"");
        let fd = c.fd().expect("fd");
        assert!(c.on_disk() && !c.file_content());
        assert_eq!(c.fd().expect("fd"), fd);
        let mut s = String::new();
        let mut f = std::fs::File::open(format!("/proc/self/fd/{fd}")).expect("open");
        f.read_to_string(&mut s).expect("read");
        assert_eq!(s, "hello world");
        assert_eq!(c.bytes().expect("bytes"), b"hello world");
        assert!(Content::default().is_empty());
        assert_eq!(Content::from(vec![1u8]).len(), 1);
        assert!(format!("{c:?}").contains("on_disk: true"));
    }

    #[test]
    fn spooled_content() {
        let mut t = get_temp().expect("temp");
        t.file_mut().write_all(b"0123456789").expect("write");
        let mut c = Content::spooled(t, 10);
        assert!(c.on_disk() && c.file_content());
        assert_eq!(c.read_at(2, 3).expect("range"), b"234");
        assert_eq!(c.bytes().expect("bytes"), b"0123456789");
        assert_eq!(c.read_at(8, 5).expect("range"), b"89");
        assert_eq!(c.to_vec(), b"0123456789");
        assert!(c.fd().is_ok());
        // A file shorter than the declared length cannot be mapped.
        let t = get_temp().expect("temp");
        let c = Content::spooled(t, 5);
        assert!(c.bytes().is_err());
        assert_eq!(c.to_vec(), Vec::<u8>::new());
        assert!(c.read_at(0, 5).is_err());
        let empty = Content {
            mem: None,
            file: None,
            len: 1,
            map: OnceCell::new(),
        };
        assert!(empty.bytes().is_err());
    }

    #[test]
    fn dup_shares_the_body() {
        let mut c = Content::memory(b"shared".to_vec());
        let mut f = c.dup_fd().expect("dup");
        let mut s = String::new();
        f.read_to_string(&mut s).expect("read");
        assert_eq!(s, "shared");
        assert!(c.on_disk());
    }

    #[test]
    fn temp_files_go_in_p_tmpdir() {
        // As `Ns_GetTemp`: P_tmpdir, whatever TMPDIR says.
        assert_eq!(temp_dir(), Path::new("/tmp"));
    }

    #[test]
    fn release_rewinds_and_truncates() {
        let mut t = get_temp().expect("temp");
        t.file_mut().write_all(b"abc").expect("write");
        let f = t.into_file();
        release_temp(f);
        let t = get_temp().expect("temp");
        assert_eq!(t.file().metadata().expect("meta").len(), 0);
    }
}
