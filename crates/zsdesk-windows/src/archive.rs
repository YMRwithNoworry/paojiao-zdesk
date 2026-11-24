//! Filesystem archive operations. A completed result is published with one non-replacing move.
use anyhow::{bail, ensure, Context, Result};
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::FILETIME,
        Storage::FileSystem::{FileTimeToLocalFileTime, MoveFileW, FILE_ATTRIBUTE_REPARSE_POINT},
    },
};
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

#[link(name = "kernel32")]
extern "system" {
    fn FileTimeToDosDateTime(time: *const FILETIME, date: *mut u16, clock: *mut u16) -> i32;
    fn DosDateTimeToFileTime(date: u16, clock: u16, time: *mut FILETIME) -> i32;
}

fn timestamp(metadata: &fs::Metadata) -> Option<zip::DateTime> {
    let ticks = metadata.last_write_time();
    let utc = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut local = FILETIME::default();
    let (mut date, mut clock) = (0, 0);
    unsafe { FileTimeToLocalFileTime(&utc, &mut local) }.ok()?;
    if unsafe { FileTimeToDosDateTime(&local, &mut date, &mut clock) } == 0 {
        return None;
    }
    zip::DateTime::try_from((date, clock)).ok()
}

fn restored_time(date: zip::DateTime) -> Option<std::time::SystemTime> {
    use windows::Win32::Storage::FileSystem::LocalFileTimeToFileTime;
    let (mut local, mut utc) = (FILETIME::default(), FILETIME::default());
    if unsafe { DosDateTimeToFileTime(date.datepart(), date.timepart(), &mut local) } == 0 {
        return None;
    }
    unsafe { LocalFileTimeToFileTime(&local, &mut utc) }.ok()?;
    let ticks = (u64::from(utc.dwHighDateTime) << 32) | u64::from(utc.dwLowDateTime);
    let unix = ticks.checked_sub(116444736000000000)?;
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_nanos(unix.checked_mul(100)?))
}

fn publish(source: &Path, parent: &Path, stem: &OsStr, suffix: &str) -> Result<PathBuf> {
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let from = wide(source);
    for index in 1u32.. {
        let mut name = stem.to_os_string();
        if index > 1 {
            name.push(format!(" ({index})"));
        }
        name.push(suffix);
        let target = parent.join(name);
        let to = wide(&target);
        match unsafe { MoveFileW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr())) } {
            Ok(()) => return Ok(target),
            Err(error) if matches!(error.code().0 as u32, 0x80070050 | 0x800700B7) => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("无法保存 {}", target.display()))
            }
        }
    }
    bail!("找不到可用的输出文件名")
}

pub fn compress_zip(paths: &[PathBuf]) -> Result<PathBuf> {
    let first = paths.first().context("请选择要压缩的文件或文件夹")?;
    let parent = first.parent().context("无法在此处生成 ZIP")?;
    ensure!(
        paths.iter().all(|p| p.parent() == Some(parent)),
        "所选项目需要位于同一文件夹"
    );
    let stem = if paths.len() == 1 {
        if first.is_dir() {
            first.file_name()
        } else {
            first.file_stem()
        }
    } else {
        parent.file_name()
    }
    .unwrap_or(OsStr::new("Archive"));
    let stage = tempfile::Builder::new()
        .prefix(".zsdesk-zip-")
        .tempdir_in(parent)?;
    let output = stage.path().join("archive.zip");
    let mut writer = ZipWriter::new(BufWriter::new(File::create_new(&output)?));
    let mut pending: Vec<_> = paths.iter().rev().cloned().collect();
    while let Some(path) = pending.pop() {
        let result = (|| -> Result<()> {
            let metadata = fs::symlink_metadata(&path)?;
            ensure!(
                metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
                "不支持将重解析点作为普通文件压缩"
            );
            let name = path
                .strip_prefix(parent)?
                .to_str()
                .context("ZIP 文件名无法表示为 Unicode")?
                .replace('\\', "/");
            let mut options =
                SimpleFileOptions::default().large_file(metadata.len() >= u32::MAX as u64);
            if let Some(date) = timestamp(&metadata) {
                options = options.last_modified_time(date);
            }
            if metadata.is_dir() {
                writer.add_directory(name, options)?;
                for entry in fs::read_dir(&path)? {
                    pending.push(entry?.path());
                }
            } else {
                ensure!(metadata.is_file(), "不支持此文件类型");
                let mut file = File::options().read(true).share_mode(1).open(&path)?;
                let expected = file.metadata()?.len();
                writer.start_file(name, options.large_file(expected >= u32::MAX as u64))?;
                ensure!(
                    io::copy(&mut file, &mut writer)? == expected,
                    "压缩期间文件大小改变"
                );
            }
            Ok(())
        })();
        result.with_context(|| format!("压缩失败：{}", path.display()))?;
    }
    let mut finished = writer.finish()?;
    finished.flush()?;
    finished.get_ref().sync_all()?;
    drop(finished);
    publish(&output, parent, stem, ".zip")
}

// Each member is created under a fresh directory. Names that Windows would reinterpret
// (device names, alternate streams, absolute paths, trailing dots/spaces) are rejected.
fn member_path(name: &str) -> Result<PathBuf> {
    ensure!(!name.starts_with(['/', '\\']), "压缩包包含绝对路径");
    let mut relative = PathBuf::new();
    for part in name
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
    {
        let base = part
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end()
            .to_uppercase();
        let device = matches!(
            base.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || base
            .strip_prefix("COM")
            .or_else(|| base.strip_prefix("LPT"))
            .is_some_and(|n| {
                n.len() == 1 && n.as_bytes()[0].is_ascii_digit() || matches!(n, "¹" | "²" | "³")
            });
        ensure!(
            !device
                && !part.ends_with([' ', '.'])
                && !part.chars().any(|c| c < ' ' || "<>:\"|?*".contains(c)),
            "压缩包内的路径在 Windows 上无效：{name}"
        );
        relative.push(part);
    }
    Ok(relative)
}

fn extract_member(
    root: &Path,
    name: &str,
    directory: bool,
    size: u64,
    input: &mut impl Read,
    modified: Option<std::time::SystemTime>,
) -> Result<()> {
    let relative = member_path(name)?;
    let path = root.join(&relative);
    if directory {
        fs::create_dir_all(path)?;
        return Ok(());
    }
    ensure!(!relative.as_os_str().is_empty(), "压缩包包含空文件名");
    fs::create_dir_all(path.parent().context("文件路径无效")?)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("无法创建压缩包中的文件：{name}"))?;
    ensure!(
        io::copy(input, &mut file)? == size,
        "文件数据不完整：{name}"
    );
    if let Some(time) = modified {
        file.set_modified(time)?;
    }
    Ok(())
}

fn unpack_zip(archive: &Path, root: &Path) -> Result<()> {
    let mut zip = ZipArchive::new(BufReader::new(File::open(archive)?))?;
    for index in 0..zip.len() {
        let mut item = zip.by_index(index)?;
        ensure!(!item.is_symlink(), "压缩包包含链接：{}", item.name());
        let name = item.name().to_owned();
        let time = item.last_modified().and_then(restored_time);
        extract_member(root, &name, item.is_dir(), item.size(), &mut item, time)?;
    }
    Ok(())
}

mod system_reader;

pub fn supported(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    [
        ".zip", ".7z", ".rar", ".tar", ".tgz", ".tbz", ".tbz2", ".txz", ".tar.gz", ".tar.bz2",
        ".tar.xz", ".tar.zst",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
}

pub fn extract_here(archive: &Path, destination: &Path) -> Result<PathBuf> {
    ensure!(
        archive.is_file()
            && archive.is_absolute()
            && destination.is_absolute()
            && supported(archive),
        "请选择支持的压缩包"
    );
    let stage = tempfile::Builder::new()
        .prefix(".zsdesk-extract-")
        .tempdir_in(destination)?;
    let contents = stage.path().join("contents");
    fs::create_dir(&contents)?;
    if archive
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
    {
        unpack_zip(archive, &contents)?;
    } else {
        system_reader::unpack(archive, &contents)?;
    }
    let mut roots = fs::read_dir(&contents)?;
    if let Some(first) = roots.next().transpose()? {
        if roots.next().transpose()?.is_none() && first.file_type()?.is_dir() {
            return publish(&first.path(), destination, &first.file_name(), "");
        }
    }
    publish(
        &contents,
        destination,
        archive.file_stem().context("压缩包文件名无效")?,
        "",
    )
}

#[cfg(test)]
mod tests;
