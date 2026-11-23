use std::{ffi::OsString, fs, io, os::windows::fs::MetadataExt, path::Path};
pub(super) struct Entry {
    pub name: OsString,
    pub folder: bool,
}
#[derive(Clone, Copy)]
pub(super) struct Visibility {
    hidden: bool,
    protected: bool,
}
impl Visibility {
    pub fn current() -> Self {
        use windows::{
            core::w,
            Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD},
        };
        let mut enabled = 0u32;
        let mut size = 4u32;
        let protected = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced"),
                w!("ShowSuperHidden"),
                RRF_RT_REG_DWORD,
                None,
                Some((&mut enabled as *mut u32).cast()),
                Some(&mut size),
            )
        }
        .is_ok()
            && enabled != 0;
        Self {
            hidden: zsdesk_windows::shell::commands::folder_settings().0,
            protected,
        }
    }
    fn includes(self, attributes: u32) -> bool {
        attributes & 2 == 0 || (self.hidden && (attributes & 4 == 0 || self.protected))
    }
}
/// Both panes apply the same visibility policy. A preview limits retained rows but counts every visible child.
pub(super) fn entries(
    path: &Path,
    visibility: Visibility,
    limit: usize,
    cancel: impl Fn() -> bool,
) -> io::Result<(Vec<Entry>, usize)> {
    let mut rows = Vec::new();
    let mut count = 0;
    for (index, entry) in fs::read_dir(path)?.enumerate() {
        if index % 128 == 0 && cancel() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "目录读取已取消"));
        }
        let entry = entry?;
        let attrs = entry.metadata()?.file_attributes();
        if !visibility.includes(attrs) {
            continue;
        }
        count += 1;
        if rows.len() < limit {
            rows.push(Entry {
                name: entry.file_name(),
                folder: attrs & 16 != 0,
            });
        }
    }
    rows.sort_by_cached_key(|entry| (!entry.folder, entry.name.to_string_lossy().to_lowercase()));
    Ok((rows, count))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parent_and_preview_follow_visibility_and_count_the_full_folder() -> io::Result<()> {
        let dir = std::env::temp_dir().join(format!("zsdesk-column-list-{}", std::process::id()));
        fs::create_dir(&dir)?;
        struct Guard(std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _guard = Guard(dir.clone());
        fs::create_dir(dir.join("data"))?;
        fs::write(dir.join("readme.txt"), b"test")?;
        let visible = Visibility {
            hidden: true,
            protected: false,
        };
        let (all, count) = entries(&dir, visible, usize::MAX, || false)?;
        assert_eq!(count, 2);
        assert!(all[0].folder);
        assert_eq!(all[0].name, "data");
        let (one, count) = entries(&dir, visible, 1, || false)?;
        assert_eq!((one.len(), count), (1, 2));
        assert!(visible.includes(2) && !visible.includes(6));
        assert!(!Visibility {
            hidden: false,
            protected: false
        }
        .includes(2));
        assert!(entries(&dir, visible, usize::MAX, || true).is_err());
        Ok(())
    }
}
