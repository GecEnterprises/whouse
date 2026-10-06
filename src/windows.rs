//! Native Windows queries. Every owned handle is closed through RAII.
use crate::scan::process_io::ObservedFile;
use crate::scan::process_io::Reading;
use crate::scan::{Access, Kind, OpenFile};
use crate::target::{Mount, Space};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::Threading::*;

pub struct Handle(pub HANDLE);
impl Handle {
    pub fn new(h: HANDLE) -> Option<Self> {
        (!h.is_null() && h != INVALID_HANDLE_VALUE).then_some(Self(h))
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns a valid handle, closed exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub fn readings(only: Option<u32>) -> (Vec<Reading>, usize, usize) {
    let (mut scanned, mut denied) = (0, 0);
    let mut rows = Vec::new();
    // SAFETY: all output structures are initialized and sized as required by Win32.
    unsafe {
        let Some(snapshot) = Handle::new(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)) else {
            return (rows, 0, 0);
        };
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        let mut ok = Process32FirstW(snapshot.0, &mut entry);
        while ok != 0 {
            let pid = entry.th32ProcessID;
            if pid != 0 && only.is_none_or(|p| p == pid) {
                scanned += 1;
                if let Some(process) =
                    Handle::new(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid))
                {
                    let mut io: IO_COUNTERS = std::mem::zeroed();
                    let (mut creation, mut exit, mut kernel, mut user): (
                        FILETIME,
                        FILETIME,
                        FILETIME,
                        FILETIME,
                    ) = std::mem::zeroed();
                    if GetProcessIoCounters(process.0, &mut io) != 0
                        && GetProcessTimes(
                            process.0,
                            &mut creation,
                            &mut exit,
                            &mut kernel,
                            &mut user,
                        ) != 0
                    {
                        let n = entry
                            .szExeFile
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(entry.szExeFile.len());
                        let name = String::from_utf16_lossy(&entry.szExeFile[..n]);
                        let mut image = vec![0u16; 32768];
                        let mut size = image.len() as u32;
                        let command = if QueryFullProcessImageNameW(
                            process.0,
                            0,
                            image.as_mut_ptr(),
                            &mut size,
                        ) != 0
                        {
                            String::from_utf16_lossy(&image[..size as usize])
                        } else {
                            name.clone()
                        };
                        rows.push(Reading {
                            pid,
                            identity: ((creation.dwHighDateTime as u64) << 32)
                                | creation.dwLowDateTime as u64,
                            ppid: entry.th32ParentProcessID,
                            name,
                            command,
                            user: "-".into(),
                            state: 'R',
                            read: io.ReadTransferCount,
                            write: io.WriteTransferCount,
                            disk_read: None,
                            disk_write: None,
                        });
                    } else if GetLastError() == ERROR_ACCESS_DENIED {
                        denied += 1;
                    }
                } else if GetLastError() == ERROR_ACCESS_DENIED {
                    denied += 1;
                }
            }
            ok = Process32NextW(snapshot.0, &mut entry);
        }
    }
    (rows, scanned, denied)
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
pub fn mount(path: &Path) -> Option<Mount> {
    let path = wide(path);
    let mut volume = vec![0u16; 32768];
    // SAFETY: NUL-terminated input and writable output buffers.
    unsafe {
        if GetVolumePathNameW(path.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) == 0 {
            return None;
        }
        let n = volume.iter().position(|&c| c == 0)?;
        let point = PathBuf::from(OsString::from_wide(&volume[..n]));
        let mut fs = [0u16; 256];
        GetVolumeInformationW(
            volume.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        );
        let n = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
        Some(Mount {
            source: point.display().to_string(),
            point,
            fstype: String::from_utf16_lossy(&fs[..n]),
        })
    }
}
pub fn space(path: &Path) -> Option<Space> {
    let dir = if path.is_file() { path.parent()? } else { path };
    let path = wide(dir);
    let (mut free, mut total) = (0, 0);
    // SAFETY: valid path and writable u64 counters.
    unsafe {
        if GetDiskFreeSpaceExW(path.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) == 0 {
            return None;
        }
    }
    Some(Space { total, free })
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SystemHandle {
    pub object: usize,
    pub pid: usize,
    pub value: usize,
    pub access: u32,
    pub trace: u16,
    pub kind: u16,
    pub attributes: u32,
    pub reserved: u32,
}
#[repr(C)]
struct IoStatus {
    status: usize,
    information: usize,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        buffer: *mut std::ffi::c_void,
        size: u32,
        needed: *mut u32,
    ) -> i32;
    fn NtQueryInformationFile(
        handle: HANDLE,
        status: *mut IoStatus,
        buffer: *mut std::ffi::c_void,
        size: u32,
        class: u32,
    ) -> i32;
}

pub fn handles() -> std::io::Result<Vec<SystemHandle>> {
    let mut bytes = 1024 * 1024usize;
    loop {
        // usize storage guarantees the alignment required by the native structures.
        let mut buffer = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
        let mut needed = 0;
        // SAFETY: aligned buffer, bounded size, valid writable length pointer.
        let status = unsafe {
            NtQuerySystemInformation(64, buffer.as_mut_ptr().cast(), bytes as u32, &mut needed)
        };
        if status == 0 {
            let count = buffer[0];
            let available =
                (bytes - 2 * std::mem::size_of::<usize>()) / std::mem::size_of::<SystemHandle>();
            if count > available {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            // SAFETY: the returned count was checked against the allocated buffer.
            return Ok(unsafe {
                std::slice::from_raw_parts(buffer.as_ptr().add(2).cast::<SystemHandle>(), count)
            }
            .to_vec());
        }
        if status != 0xc0000004u32 as i32 {
            return Err(std::io::Error::other(format!(
                "Windows handle enumeration failed (NTSTATUS {status:#x})"
            )));
        }
        bytes = bytes.saturating_mul(2).max(needed as usize);
        if bytes > 256 * 1024 * 1024 {
            return Err(std::io::Error::other("Windows handle table is too large"));
        }
    }
}

pub struct FileHandle {
    pub identity: (u32, u64),
    pub path: PathBuf,
    pub pos: Option<u64>,
    pub size: u64,
    pub dir: bool,
}
pub fn file_handle(process: &Handle, entry: SystemHandle) -> Option<FileHandle> {
    // SAFETY: source process is live, duplicate is locally owned, all buffers sized.
    unsafe {
        let mut duplicate = std::ptr::null_mut();
        if DuplicateHandle(
            process.0,
            entry.value as HANDLE,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        ) == 0
        {
            return None;
        }
        let duplicate = Handle::new(duplicate)?;
        // Do not query pipe/socket names: these can block indefinitely.
        if GetFileType(duplicate.0) != FILE_TYPE_DISK {
            return None;
        }
        let mut path = vec![0u16; 32768];
        let n = GetFinalPathNameByHandleW(
            duplicate.0,
            path.as_mut_ptr(),
            path.len() as u32,
            FILE_NAME_NORMALIZED,
        );
        if n == 0 || n as usize >= path.len() {
            return None;
        }
        let path = PathBuf::from(OsString::from_wide(&path[..n as usize]));
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        if GetFileInformationByHandle(duplicate.0, &mut info) == 0 {
            return None;
        }
        let mut pos = 0i64;
        let mut status: IoStatus = std::mem::zeroed();
        let result = NtQueryInformationFile(
            duplicate.0,
            &mut status,
            (&mut pos as *mut i64).cast(),
            8,
            14,
        );
        Some(FileHandle {
            identity: (
                info.dwVolumeSerialNumber,
                ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
            ),
            path,
            pos: (result == 0 && pos >= 0).then_some(pos as u64),
            size: ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64,
            dir: info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
        })
    }
}

pub fn open_files(readings: &[Reading]) -> (HashMap<u32, Vec<ObservedFile>>, Option<String>) {
    let probe = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::File::open(p).ok());
    let handles = match handles() {
        Ok(h) => h,
        Err(e) => return (HashMap::new(), Some(e.to_string())),
    };
    let file_kind = probe
        .as_ref()
        .and_then(|f| {
            handles.iter().find(|h| {
                h.pid == std::process::id() as usize && h.value == f.as_raw_handle() as usize
            })
        })
        .map(|h| h.kind);
    let wanted: HashSet<_> = readings.iter().map(|r| r.pid).collect();
    let mut by_pid: HashMap<u32, Vec<SystemHandle>> = HashMap::new();
    let mut holders: HashMap<usize, HashSet<usize>> = HashMap::new();
    for h in handles {
        if file_kind.is_some_and(|kind| h.kind != kind) {
            continue;
        }
        holders.entry(h.object).or_default().insert(h.pid);
        if let Ok(pid) = u32::try_from(h.pid) {
            if wanted.contains(&pid) {
                by_pid.entry(pid).or_default().push(h);
            }
        }
    }
    let mut result = HashMap::new();
    let mut unavailable = 0;
    for r in readings {
        // SAFETY: OpenProcess returns an owned handle or null.
        let Some(process) = Handle::new(unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, r.pid) })
        else {
            unavailable += 1;
            continue;
        };
        let mut files: Vec<ObservedFile> = Vec::new();
        let mut groups: HashMap<((u32, u64), Option<u64>, u32), usize> = HashMap::new();
        for h in by_pid.remove(&r.pid).unwrap_or_default() {
            let Some(f) = file_handle(&process, h) else {
                continue;
            };
            // The source handle can be recycled after enumeration. Group by
            // the identity actually queried from the duplicate, not the old
            // object pointer in the system snapshot.
            let group = (f.identity, f.pos, h.access);
            if let Some(&index) = groups.get(&group) {
                files[index].file.fds.push(h.value as u32);
                continue;
            }
            groups.insert(group, files.len());
            let access = match (h.access & 1 != 0, h.access & 6 != 0) {
                (true, false) => Access::Read,
                (false, true) => Access::Write,
                _ => Access::ReadWrite,
            };
            files.push(ObservedFile {
                key: (
                    f.identity.1 ^ (f.identity.0 as u64).rotate_left(32),
                    h.value as u64,
                ),
                position: f.pos,
                file: OpenFile {
                    fds: vec![h.value as u32],
                    path: f.path,
                    kind: if f.dir {
                        Kind::Dir
                    } else if h.access & 7 == 0 {
                        Kind::Other
                    } else {
                        Kind::File
                    },
                    access,
                    pos: f.pos.unwrap_or(0),
                    size: f.size,
                    read_bps: 0.0,
                    write_bps: 0.0,
                    deleted: false,
                    shared: holders.get(&h.object).is_some_and(|p| p.len() > 1),
                },
            });
        }
        result.insert(r.pid, files);
    }
    (result, (unavailable > 0).then(|| format!("File handles unavailable for {unavailable} processes; their I/O counters are still monitored")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn volume_and_free_space_are_available() {
        let cwd = std::fs::canonicalize(".").unwrap();
        assert!(!mount(&cwd).unwrap().fstype.is_empty());
        let s = space(&cwd).unwrap();
        assert!(s.total > 0 && s.free <= s.total);
    }
}
