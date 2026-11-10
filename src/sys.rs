// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Platform code: positional I/O, opens that refuse links and FIFOs, the
//! directory steps of `extract`, temp file ownership, signal cleanup,
//! free memory and terminal size. Unix and Windows halves have the same API.

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod unix {
    use std::ffi::{CString, OsStr};
    use std::fs::{self, File};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    pub use std::os::unix::fs::FileExt;

    /// Opens a file found by a directory walk for reading. A symlink fails
    /// (O_NOFOLLOW); O_NONBLOCK keeps a FIFO from blocking the open and does
    /// nothing for a regular file.
    pub fn open_source_file(p: &Path) -> io::Result<File> {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(p)
    }

    /// Opens `p` for reading without blocking on a FIFO with no writer.
    pub fn open_nonblock(p: &Path) -> io::Result<File> {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(p)
    }

    /// Mode of a new temp file: private if it will take an old file's mode.
    pub fn temp_mode(o: &mut fs::OpenOptions, replaces_file: bool) {
        o.mode(if replaces_file { 0o600 } else { 0o666 });
    }

    /// Gives `f` the owner (best effort) and mode of `old`.
    pub fn copy_owner_mode(f: &File, old: &fs::Metadata) -> io::Result<()> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // only root can give a file away; others keep their own owner
        let _ = std::os::unix::fs::fchown(f, Some(old.uid()), Some(old.gid()));
        f.set_permissions(fs::Permissions::from_mode(old.mode() & 0o777))
    }

    /// An open directory: each step of `extract` is relative to one.
    pub type Dir = File;

    pub fn open_root(p: &Path) -> io::Result<Dir> {
        File::open(p)
    }

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| io::Error::other("name contains NUL"))
    }

    fn fd_result(fd: libc::c_int) -> io::Result<File> {
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat just returned this descriptor and nothing else owns it
        Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// Opens directory `name` in `dir`, creating it if missing. A symlink in its
    /// place fails (O_NOFOLLOW), so extraction cannot be sent outside `dir`.
    pub fn open_dir_at(dir: &Dir, name: &OsStr) -> io::Result<Dir> {
        let c = c_name(name)?;
        // SAFETY: valid descriptor and NUL-terminated name
        if unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o777) } != 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::AlreadyExists {
                return Err(err);
            }
        }
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: as above
        fd_result(unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) })
    }

    /// Creates file `name` in `dir`. O_EXCL fails on any existing name, a
    /// symlink too, so the file written is always a new one.
    pub fn create_new_at(dir: &Dir, name: &OsStr) -> io::Result<File> {
        let c = c_name(name)?;
        let flags =
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: valid descriptor and NUL-terminated name
        fd_result(unsafe {
            libc::openat(dir.as_raw_fd(), c.as_ptr(), flags, 0o666 as libc::c_uint)
        })
    }

    /// Renames `from` to `to`, both in `dir`. Whatever is at `to` (a symlink,
    /// or a hard link to someone else's file) is replaced, never written into.
    pub fn rename_at(dir: &Dir, from: &OsStr, to: &OsStr) -> io::Result<()> {
        let (f, t) = (c_name(from)?, c_name(to)?);
        let fd = dir.as_raw_fd();
        // SAFETY: valid descriptor and NUL-terminated names
        if unsafe { libc::renameat(fd, f.as_ptr(), fd, t.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Removes file `name` from `dir`, by name in the open directory, never
    /// through a path.
    pub fn unlink_at(dir: &Dir, name: &OsStr) -> io::Result<()> {
        let c = c_name(name)?;
        // SAFETY: valid descriptor and NUL-terminated name
        if unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// What tells one file from every other: device and inode.
    pub type FileId = (libc::dev_t, libc::ino_t);

    /// Identity of what `name` is in `dir` (of a symlink, not of its
    /// target), None if nothing is there.
    pub fn file_id_at(dir: &Dir, name: &OsStr) -> io::Result<Option<FileId>> {
        let c = c_name(name)?;
        // SAFETY: plain data, all zeros is valid
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let flags = libc::AT_SYMLINK_NOFOLLOW;
        // SAFETY: valid descriptor, NUL-terminated name and valid pointer
        if unsafe { libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut st, flags) } != 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(err)
            };
        }
        Ok(Some((st.st_dev, st.st_ino)))
    }

    /// On SIGINT, SIGTERM, SIGHUP or SIGQUIT, removes unfinished temp files
    /// and exits with 128 + signal (SIGQUIT then leaves no core dump). Runs
    /// on its own thread, so the cleanup is not limited to what a signal
    /// handler may do.
    pub fn cleanup_on_signal() {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
        let sigs = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT]);
        let mut sigs = match sigs {
            Ok(s) => s,
            Err(e) => {
                crate::progress::print_line(
                    true,
                    &format!("warning: no signal handler, temp files may be left: {e}"),
                );
                return;
            }
        };
        std::thread::spawn(move || {
            if let Some(sig) = sigs.forever().next() {
                crate::zipwrite::remove_temp_files_and_exit(128 + sig);
            }
        });
    }

    /// MemAvailable in MiB, or None if unknown.
    pub fn mem_available_mib() -> Option<u64> {
        fs::read_to_string("/proc/meminfo").ok().and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemAvailable:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                .map(|kb| kb / 1024)
        })
    }

    /// Columns and rows of the terminal on stderr.
    pub fn term_size() -> Option<(u16, u16)> {
        // SAFETY: plain data, all zeros is valid
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ fills a winsize through a valid pointer
        let r = unsafe { libc::ioctl(2, libc::TIOCGWINSZ, &mut ws) };
        (r == 0 && ws.ws_col > 0 && ws.ws_row > 0).then_some((ws.ws_col, ws.ws_row))
    }

    /// Terminals take ANSI escapes as they are.
    pub fn enable_vt(_stderr: bool) -> bool {
        true
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsStr;
    use std::fs::{self, File};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    /// Positional I/O like `std::os::unix::fs::FileExt`. Unlike pread and
    /// pwrite, these move the file cursor: seek before the next plain write.
    pub trait FileExt {
        fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<usize>;
        fn write_at(&self, buf: &[u8], off: u64) -> io::Result<usize>;

        fn read_exact_at(&self, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
            while !buf.is_empty() {
                match self.read_at(buf, off) {
                    Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(n) => {
                        buf = &mut buf[n..];
                        off += n as u64;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        fn write_all_at(&self, mut buf: &[u8], mut off: u64) -> io::Result<()> {
            while !buf.is_empty() {
                match self.write_at(buf, off) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(n) => {
                        buf = &buf[n..];
                        off += n as u64;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }

    impl FileExt for File {
        fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<usize> {
            std::os::windows::fs::FileExt::seek_read(self, buf, off)
        }
        fn write_at(&self, buf: &[u8], off: u64) -> io::Result<usize> {
            std::os::windows::fs::FileExt::seek_write(self, buf, off)
        }
    }

    /// Opens a file found by a directory walk for reading. A symlink opens
    /// as itself (FILE_FLAG_OPEN_REPARSE_POINT), so the caller's `is_file`
    /// check rejects it; a directory fails to open.
    pub fn open_source_file(p: &Path) -> io::Result<File> {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(p)
    }

    /// A named pipe does not block the open on Windows: plain open.
    pub fn open_nonblock(p: &Path) -> io::Result<File> {
        File::open(p)
    }

    /// No Unix mode bits on Windows.
    pub fn temp_mode(_: &mut fs::OpenOptions, _: bool) {}

    /// The new file keeps the default ACL of its directory.
    pub fn copy_owner_mode(_: &File, _: &fs::Metadata) -> io::Result<()> {
        Ok(())
    }

    /// Windows has no openat: `extract` steps go by path. Each step checks
    /// that what it opens is not a link, but a link swapped in between the
    /// check and the next step is followed, unlike the Unix version.
    pub type Dir = PathBuf;

    pub fn open_root(p: &Path) -> io::Result<Dir> {
        Ok(p.to_path_buf())
    }

    /// Creates directory `name` in `dir` if missing. A symlink or junction
    /// in its place is refused.
    pub fn open_dir_at(dir: &Dir, name: &OsStr) -> io::Result<Dir> {
        let p = dir.join(name);
        if let Err(err) = fs::create_dir(&p)
            && err.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(err);
        }
        // is_dir is false for a symlink or junction
        if !fs::symlink_metadata(&p)?.is_dir() {
            return Err(io::Error::other("not a directory"));
        }
        Ok(p)
    }

    /// Creates file `name` in `dir`. `create_new` fails on any existing
    /// name and does not follow a link.
    pub fn create_new_at(dir: &Dir, name: &OsStr) -> io::Result<File> {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(dir.join(name))
    }

    /// Renames `from` to `to`, both in `dir`. Whatever is at `to` (a symlink,
    /// or a hard link to someone else's file) is replaced, never written into.
    pub fn rename_at(dir: &Dir, from: &OsStr, to: &OsStr) -> io::Result<()> {
        fs::rename(dir.join(from), dir.join(to))
    }

    pub fn unlink_at(dir: &Dir, name: &OsStr) -> io::Result<()> {
        fs::remove_file(dir.join(name))
    }

    /// What tells one file from every other: volume serial number and file
    /// index.
    pub type FileId = (u32, u64);

    fn file_id(f: &File) -> io::Result<FileId> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        // SAFETY: plain data, all zeros is valid
        let mut i: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: valid handle and pointer
        if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut i) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let index = u64::from(i.nFileIndexHigh) << 32 | u64::from(i.nFileIndexLow);
        Ok((i.dwVolumeSerialNumber, index))
    }

    /// Identity of what `name` is in `dir` (of a symlink, not of its
    /// target), None if nothing is there.
    pub fn file_id_at(dir: &Dir, name: &OsStr) -> io::Result<Option<FileId>> {
        // no access rights asked: a file in use opens too. A directory
        // needs FILE_FLAG_BACKUP_SEMANTICS
        match fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir.join(name))
        {
            Ok(f) => file_id(&f).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// On Ctrl+C, Ctrl+Break or console close, removes unfinished temp
    /// files and exits with STATUS_CONTROL_C_EXIT. Windows runs the handler
    /// on its own thread.
    pub fn cleanup_on_signal() {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        unsafe extern "system" fn handler(_: u32) -> windows_sys::core::BOOL {
            crate::zipwrite::remove_temp_files_and_exit(0xC000_013A_u32 as i32);
        }
        // SAFETY: handler is a valid function for the whole process life
        if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
            crate::progress::print_line(
                true,
                &format!(
                    "warning: no signal handler, temp files may be left: {}",
                    io::Error::last_os_error()
                ),
            );
        }
    }

    /// Available physical memory in MiB, or None if unknown.
    pub fn mem_available_mib() -> Option<u64> {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: plain data, all zeros is valid
        let mut s: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        s.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: valid pointer with dwLength set
        let ok = unsafe { GlobalMemoryStatusEx(&mut s) } != 0;
        ok.then_some(s.ullAvailPhys / (1024 * 1024))
    }

    /// Columns and rows of the console window on stderr.
    pub fn term_size() -> Option<(u16, u16)> {
        use windows_sys::Win32::System::Console::{
            CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_ERROR_HANDLE,
        };
        // SAFETY: plain data, all zeros is valid
        let mut i: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: valid pointer; a bad handle only makes the call fail
        let ok = unsafe { GetConsoleScreenBufferInfo(GetStdHandle(STD_ERROR_HANDLE), &mut i) } != 0;
        let w = i.srWindow;
        let (c, r) = (w.Right - w.Left + 1, w.Bottom - w.Top + 1);
        (ok && c > 0 && r > 0).then_some((c as u16, r as u16))
    }

    /// Turns on ANSI escapes in the console on stderr (or stdout); false if
    /// not a console.
    pub fn enable_vt(stderr: bool) -> bool {
        use windows_sys::Win32::System::Console::{
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE,
            STD_OUTPUT_HANDLE, SetConsoleMode,
        };
        let std = if stderr {
            STD_ERROR_HANDLE
        } else {
            STD_OUTPUT_HANDLE
        };
        // SAFETY: plain handle calls; a bad handle only makes them fail
        unsafe {
            let h = GetStdHandle(std);
            let mut mode = 0;
            GetConsoleMode(h, &mut mode) != 0
                && SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
        }
    }
}
