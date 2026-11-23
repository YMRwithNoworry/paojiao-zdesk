//! Stream regular members from the Windows archive library without extracting links or special files.
use super::*;
use libloading::os::windows::Library;
use std::ffi::{c_char, c_int, c_void, CStr};

type Handle = *mut c_void;
type Status = unsafe extern "C" fn(Handle) -> c_int;
type Data = unsafe extern "C" fn(Handle, *mut c_void, usize) -> isize;
type ErrorText = unsafe extern "C" fn(Handle) -> *const c_char;

struct Reader {
    handle: Handle,
    free: Status,
    data: Data,
    error: ErrorText,
}
impl Reader {
    fn detail(&self) -> String {
        let raw = unsafe { (self.error)(self.handle) };
        if raw.is_null() {
            return "无法读取压缩包".into();
        }
        unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned()
    }
    fn check(&self, code: c_int) -> Result<()> {
        ensure!(code == 0, "{}", self.detail());
        Ok(())
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        unsafe {
            (self.free)(self.handle);
        }
    }
}
impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = unsafe { (self.data)(self.handle, buffer.as_mut_ptr().cast(), buffer.len()) };
        usize::try_from(count).map_err(|_| io::Error::other(self.detail()))
    }
}

pub(super) fn unpack(path: &Path, root: &Path) -> Result<()> {
    // Load only from System32. Older systems can still use the independent ZIP reader.
    let library = unsafe { Library::load_with_flags("archiveint.dll", 0x800) }
        .context("此 Windows 版本缺少相应的压缩格式支持")?;
    macro_rules! api {
        ($name:literal, $ty:ty) => {
            *unsafe { library.get::<$ty>(concat!($name, "\0").as_bytes()) }?
        };
    }
    let version = api!("archive_version_number", unsafe extern "C" fn() -> c_int);
    ensure!(
        (3_000_000..3_999_000).contains(&unsafe { version() }),
        "Windows 解压库版本不兼容"
    );
    let new = api!("archive_read_new", unsafe extern "C" fn() -> Handle);
    let free = api!("archive_read_free", Status);
    let data = api!("archive_read_data", Data);
    let error = api!("archive_error_string", ErrorText);
    let mut reader = Reader {
        handle: unsafe { new() },
        free,
        data,
        error,
    };
    ensure!(!reader.handle.is_null(), "无法创建解压器");
    let filters = api!("archive_read_support_filter_all", Status);
    let formats = api!("archive_read_support_format_all", Status);
    // A warning here indicates that some optional formats are unavailable.
    ensure!(
        unsafe { filters(reader.handle) } >= -20 && unsafe { formats(reader.handle) } >= -20,
        "{}",
        reader.detail()
    );
    let options = api!(
        "archive_read_set_format_option",
        unsafe extern "C" fn(Handle, *const c_char, *const c_char, *const c_char) -> c_int
    );
    reader.check(unsafe {
        options(
            reader.handle,
            c"tar".as_ptr(),
            c"hdrcharset".as_ptr(),
            c"UTF-8".as_ptr(),
        )
    })?;
    let open = api!(
        "archive_read_open_filename_w",
        unsafe extern "C" fn(Handle, *const u16, usize) -> c_int
    );
    let next = api!(
        "archive_read_next_header",
        unsafe extern "C" fn(Handle, *mut Handle) -> c_int
    );
    let name = api!(
        "archive_entry_pathname_w",
        unsafe extern "C" fn(Handle) -> *const u16
    );
    let hardlink = api!(
        "archive_entry_hardlink_w",
        unsafe extern "C" fn(Handle) -> *const u16
    );
    let filetype = api!(
        "archive_entry_filetype",
        unsafe extern "C" fn(Handle) -> u16
    );
    let size = api!("archive_entry_size", unsafe extern "C" fn(Handle) -> i64);
    let mtime = api!("archive_entry_mtime", unsafe extern "C" fn(Handle) -> i64);
    let has_mtime = api!(
        "archive_entry_mtime_is_set",
        unsafe extern "C" fn(Handle) -> c_int
    );
    let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    reader.check(unsafe { open(reader.handle, path.as_ptr(), 64 * 1024) })?;
    loop {
        let mut entry = std::ptr::null_mut();
        let code = unsafe { next(reader.handle, &mut entry) };
        if code == 1 {
            break;
        } // ARCHIVE_EOF
        reader.check(code)?;
        ensure!(!entry.is_null(), "缺少压缩包目录项");
        let raw = PCWSTR(unsafe { name(entry) });
        ensure!(!raw.is_null(), "无法识别压缩包内的文件名");
        let path = unsafe { raw.to_string() }?;
        let kind = unsafe { filetype(entry) };
        ensure!(
            matches!(kind, 0o100000 | 0o040000) && unsafe { hardlink(entry) }.is_null(),
            "压缩包包含链接或特殊文件：{path}"
        );
        let size = u64::try_from(unsafe { size(entry) }).context("文件长度无效")?;
        let time = (unsafe { has_mtime(entry) } != 0)
            .then(|| unsafe { mtime(entry) })
            .and_then(|s| u64::try_from(s).ok())
            .and_then(|s| std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(s)));
        extract_member(root, &path, kind == 0o040000, size, &mut reader, time)?;
    }
    Ok(())
}
