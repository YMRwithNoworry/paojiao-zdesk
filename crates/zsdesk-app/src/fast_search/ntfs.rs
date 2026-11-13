//! Read-only, bounded NTFS FSCTL transport. No journal creation or privilege changes.

use std::{ffi::OsString, mem::size_of, os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf}, sync::atomic::{AtomicBool, Ordering}, time::{Duration, Instant}};
use anyhow::{bail, ensure, Context, Result};
use windows::{core::PCWSTR, Win32::{
    Foundation::{CloseHandle, HANDLE, GENERIC_READ, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::{CreateFileW, GetFileInformationByHandle, GetVolumeInformationW,
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_FLAG_OVERLAPPED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING},
    System::{IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED},
        Ioctl::{FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
            MFT_ENUM_DATA_V0, READ_USN_JOURNAL_DATA_V0},
        Threading::{CreateEventW, ResetEvent, WaitForSingleObject}},
}};

const BUFFER_BYTES: usize = 256 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(4);
pub const DIRECTORY: u32 = 0x10;
pub const REPARSE: u32 = 0x400;
pub const FILE_DELETE: u32 = 0x200;
pub const RENAME_OLD: u32 = 0x1000;
pub const RENAME_NEW: u32 = 0x2000;
pub const HARD_LINK_CHANGE: u32 = 0x10000;

struct Handle(HANDLE);
impl Drop for Handle { fn drop(&mut self) { let _ = unsafe { CloseHandle(self.0) }; } }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Journal { pub id: u64, pub first: i64, pub next: i64, pub lowest_valid: i64 }
impl Journal {
    pub fn contains(&self, id: u64, cursor: i64) -> bool {
        self.id == id && cursor >= self.first.max(self.lowest_valid) && cursor <= self.next
    }
}

#[derive(Clone, Debug)]
pub struct Record {
    pub id: u64, pub parent: u64, pub usn: i64, pub reason: u32,
    pub attributes: u32, pub name: OsString,
}
pub struct Page { pub next: u64, pub records: Vec<Record> }

fn u16_at(bytes: &[u8], at: usize) -> u16 { u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) }
fn u32_at(bytes: &[u8], at: usize) -> u32 { u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) }
fn u64_at(bytes: &[u8], at: usize) -> u64 { u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) }

/// Accept only the NTFS V2 layout; unknown versions never truncate file identities.
pub fn parse_page(bytes: &[u8]) -> Result<Page> {
    parse_records(bytes,false)
}
fn parse_records(bytes:&[u8],nameless:bool)->Result<Page> {
    ensure!(bytes.len() >= 8 && bytes.len() <= BUFFER_BYTES, "NTFS 返回缓冲区长度无效");
    let mut records = Vec::new();
    let mut offset = 8;
    while offset < bytes.len() {
        ensure!(bytes.len() - offset >= 60, "NTFS 记录头不完整");
        let record = &bytes[offset..];
        let length = u32_at(record, 0) as usize;
        ensure!(length >= 60 && length <= record.len() && length % 8 == 0, "NTFS 记录长度无效");
        ensure!(u16_at(record, 4) == 2, "此 NTFS 记录版本不受支持，使用目录补全");
        let name_bytes = u16_at(record, 56) as usize;
        let name_offset = u16_at(record, 58) as usize;
        ensure!((nameless&&name_bytes==0)||(name_bytes > 0 && name_bytes <= 510 && name_bytes % 2 == 0 && name_offset >= 60
            && name_offset % 2 == 0 && name_offset <= length && name_bytes <= length - name_offset),
            "NTFS 名称边界无效");
        let name=if name_bytes==0{&[][..]}else{&record[name_offset..name_offset+name_bytes]};
        let units: Vec<u16> = name.chunks_exact(2)
            .map(|v| u16::from_le_bytes([v[0], v[1]])).collect();
        ensure!(!units.iter().any(|n| matches!(*n, 0 | 47 | 92)), "NTFS 名称包含路径分隔符");
        let usn = u64_at(record, 24) as i64;
        ensure!(usn >= 0, "NTFS USN 无效");
        records.push(Record { id: u64_at(record, 8), parent: u64_at(record, 16), usn,
            reason: u32_at(record, 40), attributes: u32_at(record, 52), name: OsString::from_wide(&units) });
        offset += length;
    }
    Ok(Page { next: u64_at(bytes, 0), records })
}

pub struct Volume {
    pub root: PathBuf, pub serial: u32, pub root_id: u64,
    handle: Handle, event: Handle, buffer: Vec<u8>,
    unprivileged:bool,
}
impl Volume {
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_mode(root,false)
    }
    /// Opens a root directory handle, not a raw-volume handle. Windows permits
    /// journal identity queries and redacted USN reads on this ordinary handle.
    pub fn open_changes(root:&Path)->Result<Self>{Self::open_mode(root,true)}
    fn open_mode(root:&Path,unprivileged:bool)->Result<Self>{
        let text: Vec<u16> = root.as_os_str().encode_wide().collect();
        ensure!(text.len() == 3 && text[1] == b':' as u16 && text[2] == b'\\' as u16
            && text[0] < 128 && (text[0] as u8).is_ascii_alphabetic(), "仅整卷本地盘符可使用 NTFS 元数据");
        let path: Vec<u16> = text.iter().copied().chain(Some(0)).collect();
        let mut fs_name = [0u16; 32];
        let mut serial = 0;
        unsafe { GetVolumeInformationW(PCWSTR(path.as_ptr()), None, Some(&mut serial), None, None, Some(&mut fs_name)) }
            .context("无法读取卷类型")?;
        ensure!(String::from_utf16_lossy(&fs_name[..fs_name.iter().position(|n| *n == 0).unwrap_or(fs_name.len())]) == "NTFS",
            "非 NTFS 卷使用目录扫描");
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
        let root_handle = Handle(unsafe { CreateFileW(PCWSTR(path.as_ptr()), FILE_READ_ATTRIBUTES.0, share, None,
            OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, None) }.context("无法读取卷根目录标识")?);
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(root_handle.0, &mut info) }?;
        ensure!(info.dwVolumeSerialNumber == serial && info.dwFileAttributes & REPARSE == 0, "卷根目录标识已变化");
        let device: Vec<u16> = format!("\\\\.\\{}:", char::from_u32(text[0] as u32).unwrap()).encode_utf16().chain(Some(0)).collect();
        let handle = Handle(unsafe { CreateFileW(PCWSTR(if unprivileged{path.as_ptr()}else{device.as_ptr()}),
            if unprivileged{FILE_READ_ATTRIBUTES.0}else{GENERIC_READ.0}, share, None,
            OPEN_EXISTING, FILE_FLAG_OVERLAPPED|if unprivileged{FILE_FLAG_BACKUP_SEMANTICS}else{Default::default()}, None) }.context("无法只读打开 NTFS 卷；未请求提升权限")?);
        let event = Handle(unsafe { CreateEventW(None, true, false, None) }?);
        Ok(Self { root: root.to_path_buf(), serial,
            root_id: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            handle, event, buffer: vec![0; BUFFER_BYTES],unprivileged })
    }

    fn ioctl<T>(&mut self, code: u32, input: Option<&T>, stop: &AtomicBool) -> Result<usize> {
        ensure!(!stop.load(Ordering::Acquire), "索引已取消");
        unsafe { ResetEvent(self.event.0) }?;
        let mut overlapped = OVERLAPPED { hEvent: self.event.0, ..Default::default() };
        let mut returned = 0;
        let result = unsafe { DeviceIoControl(self.handle.0, code,
            input.map(|p| (p as *const T).cast()), input.map_or(0, |_| size_of::<T>() as u32),
            Some(self.buffer.as_mut_ptr().cast()), self.buffer.len() as u32, None, Some(&mut overlapped)) };
        let pending = match result {
            Ok(()) => false,
            Err(error) if error.code().0 as u32 == 0x800703E5 => true, // ERROR_IO_PENDING
            Err(error) if error.code().0 as u32 == 0x800700EA => false, // ERROR_MORE_DATA
            Err(error) => return Err(error.into()),
        };
        if pending {
            let started = Instant::now();
            loop {
                let waited = unsafe { WaitForSingleObject(self.event.0, 40) };
                if waited == WAIT_OBJECT_0 {
                    break;
                }
                if waited != WAIT_TIMEOUT || stop.load(Ordering::Acquire) || started.elapsed() >= IO_TIMEOUT {
                    // Completion is drained before stack/buffer storage is released.
                    // A broken driver may delay this worker, never the UI thread.
                    let _ = unsafe { CancelIoEx(self.handle.0, Some(&overlapped)) };
                    let _ = unsafe { GetOverlappedResult(self.handle.0, &overlapped, &mut returned, true) };
                    bail!("NTFS 读取已取消或超时，保留目录扫描回退");
                }
            }
        }
        if let Err(error) = unsafe { GetOverlappedResult(self.handle.0, &overlapped, &mut returned, false) } {
            if error.code().0 as u32 != 0x800700EA || returned == 0 { return Err(error.into()); }
        }
        ensure!(returned as usize <= self.buffer.len(), "NTFS 返回长度超出缓冲区");
        Ok(returned as usize)
    }

    pub fn journal(&mut self, stop: &AtomicBool) -> Result<Journal> {
        let length = self.ioctl::<u8>(FSCTL_QUERY_USN_JOURNAL, None, stop).context("USN 日志不可读取；不会创建或扩大日志")?;
        ensure!(length >= 56, "USN 日志信息不完整");
        let info = Journal { id: u64_at(&self.buffer, 0), first: u64_at(&self.buffer, 8) as i64,
            next: u64_at(&self.buffer, 16) as i64, lowest_valid: u64_at(&self.buffer, 24) as i64 };
        ensure!(info.first >= 0 && info.lowest_valid >= 0 && info.next >= info.first.max(info.lowest_valid), "USN 日志范围无效");
        Ok(info)
    }

    pub fn enumerate(&mut self, cursor: u64, stop: &AtomicBool) -> Result<Option<Page>> {
        ensure!(!self.unprivileged,"普通权限句柄不用于 MFT 首建");
        let input = MFT_ENUM_DATA_V0 { StartFileReferenceNumber: cursor, LowUsn: 0, HighUsn: i64::MAX };
        let length = match self.ioctl(FSCTL_ENUM_USN_DATA, Some(&input), stop) {
            Ok(length) => length,
            Err(error) if error.downcast_ref::<windows::core::Error>().is_some_and(|e| e.code().0 as u32 == 0x80070026) => return Ok(None), // ERROR_HANDLE_EOF
            Err(error) => return Err(error.context("NTFS 元数据枚举失败")),
        };
        let page = parse_page(&self.buffer[..length])?;
        ensure!(page.next > cursor, "NTFS 枚举游标未向前移动");
        Ok(Some(page))
    }

    pub fn changes(&mut self, cursor: i64, journal_id: u64, stop: &AtomicBool) -> Result<Page> {
        let input = READ_USN_JOURNAL_DATA_V0 { StartUsn: cursor, ReasonMask: u32::MAX, ReturnOnlyOnClose: 0,
            Timeout: 0, BytesToWaitFor: 0, UsnJournalID: journal_id };
        let code=if self.unprivileged{FSCTL_READ_UNPRIVILEGED_USN_JOURNAL}else{FSCTL_READ_USN_JOURNAL};
        let length = self.ioctl(code, Some(&input), stop).context("USN 增量读取失败")?;
        let page = parse_records(&self.buffer[..length],self.unprivileged)?;
        ensure!(page.next <= i64::MAX as u64 && page.next as i64 >= cursor, "USN 游标无效");
        ensure!(page.records.iter().all(|r| r.usn >= cursor && r.usn < page.next as i64), "USN 记录超出提交范围");
        ensure!(page.records.windows(2).all(|pair| pair[0].usn <= pair[1].usn), "USN 记录次序无效");
        Ok(page)
    }

    pub fn path_for_id(&self,id:u64)->Result<Option<PathBuf>>{
        use windows::Win32::Storage::FileSystem::{OpenFileById,FILE_ID_DESCRIPTOR,FILE_ID_DESCRIPTOR_0,FileIdType,GetFinalPathNameByHandleW,GETFINALPATHNAMEBYHANDLE_FLAGS};
        let descriptor=FILE_ID_DESCRIPTOR{dwSize:size_of::<FILE_ID_DESCRIPTOR>()as u32,Type:FileIdType,Anonymous:FILE_ID_DESCRIPTOR_0{FileId:id as i64}};
        let handle=match unsafe{OpenFileById(self.handle.0,&descriptor,FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,None,FILE_FLAG_BACKUP_SEMANTICS|FILE_FLAG_OPEN_REPARSE_POINT)}{
            // NTFS answers a deleted or recycled identity with ERROR_INVALID_PARAMETER, not "not found".
            Ok(handle)=>Handle(handle),Err(error)if matches!(error.code().0 as u32,0x80070002|0x80070003|0x80070490|0x80070057)=>return Ok(None),Err(error)=>return Err(error.into()),
        };
        let mut text=vec![0u16;32768];let n=unsafe{GetFinalPathNameByHandleW(handle.0,&mut text,GETFINALPATHNAMEBYHANDLE_FLAGS(0))}as usize;
        ensure!(n>0&&n<text.len(),"无法解析变更文件的当前路径");
        let value=OsString::from_wide(&text[..n]);let value=PathBuf::from(value);
        Ok(Some(super::path_key_native(&value)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(name: &[u16]) -> Vec<u8> {
        let length = (60 + name.len() * 2 + 7) & !7;
        let mut bytes = vec![0; 8 + length];
        bytes[..8].copy_from_slice(&99u64.to_le_bytes());
        bytes[8..12].copy_from_slice(&(length as u32).to_le_bytes());
        bytes[12..14].copy_from_slice(&2u16.to_le_bytes());
        bytes[16..24].copy_from_slice(&0xFFFE_0000_0000_0010u64.to_le_bytes());
        bytes[24..32].copy_from_slice(&5u64.to_le_bytes());
        bytes[64..66].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
        bytes[66..68].copy_from_slice(&60u16.to_le_bytes());
        for (i, value) in name.iter().enumerate() { bytes[68 + i * 2..70 + i * 2].copy_from_slice(&value.to_le_bytes()); }
        bytes
    }
    #[test]
    fn retains_full_sequence_id_and_lossless_names() {
        let units = [0x4E2D, 0x6587, 0xD800];
        let parsed = parse_page(&record(&units)).unwrap();
        assert_eq!(parsed.records[0].id, 0xFFFE_0000_0000_0010);
        assert_eq!(parsed.records[0].name, OsString::from_wide(&units));
    }
    #[test]
    fn malformed_records_and_unknown_versions_fail_closed() {
        let valid = record(&[b'a' as u16]);
        for length in 0..valid.len() { assert!(parse_page(&valid[..length]).is_err() || length == 8); }
        let mut bad = valid.clone(); bad[12..14].copy_from_slice(&3u16.to_le_bytes()); assert!(parse_page(&bad).is_err());
        let mut bad = valid; bad[66..68].copy_from_slice(&65534u16.to_le_bytes()); assert!(parse_page(&bad).is_err());
        assert!(parse_page(&record(&[b'\\' as u16])).is_err());
    }
    #[test]
    fn journal_checkpoint_requires_same_identity_and_retained_range() {
        let journal = Journal { id: 7, first: 50, next: 100, lowest_valid: 60 };
        assert!(journal.contains(7, 60)); assert!(journal.contains(7, 100));
        assert!(!journal.contains(8, 80)); assert!(!journal.contains(7, 59)); assert!(!journal.contains(7, 101));
    }
    #[test]
    fn deleted_file_ids_resolve_to_none() -> Result<()> {
        let dir = std::fs::canonicalize(std::env::temp_dir())?.join(format!("zsdesk-stale-id-{}", std::process::id()));
        std::fs::create_dir(&dir)?;
        let identity = super::super::native_scan::identity(&dir);
        let reader = super::super::native_scan::volume_root(&dir).and_then(|root| Volume::open_changes(&root).ok());
        let alive = reader.as_ref().zip(identity.as_ref().ok()).map(|(reader, (_, id, _))| reader.path_for_id(*id));
        std::fs::remove_dir(&dir)?;
        // Not an NTFS drive-letter volume: file identities are not used there.
        let (Some(reader), Ok((_, id, _))) = (reader, identity) else { return Ok(()); };
        assert!(alive.unwrap()?.is_some());
        assert!(reader.path_for_id(id)?.is_none(), "a deleted identity must read as gone, not as a failed catchup");
        Ok(())
    }
    #[test]
    fn ordinary_usn_records_may_omit_names_but_mft_records_may_not(){
        let mut bytes=record(&[b'x' as u16]);bytes[64..66].copy_from_slice(&0u16.to_le_bytes());bytes[66..68].copy_from_slice(&0u16.to_le_bytes());
        assert!(parse_page(&bytes).is_err());let page=parse_records(&bytes,true).unwrap();assert!(page.records[0].name.is_empty());
        bytes[12..14].copy_from_slice(&3u16.to_le_bytes());assert!(parse_records(&bytes,true).is_err());
    }
}
