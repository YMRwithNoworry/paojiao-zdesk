use super::{pane_input, WindowState};
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::Foundation::HWND;
use zsdesk_windows::{archive, shell::file_transfer};

static ACTIVE: AtomicUsize = AtomicUsize::new(0);
pub(crate) fn pending_operations() -> usize {
    ACTIVE.load(Ordering::Acquire)
}

struct Operation<'a>(&'a AtomicUsize);
impl<'a> Operation<'a> {
    fn acquire(active: &'a AtomicUsize) -> Option<Self> {
        // Match the file-transfer worker limit across all file windows.
        active.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < 4).then_some(n + 1)).ok()?;
        Some(Self(active))
    }
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl WindowState {
    pub(super) fn extract_selected_archives(&mut self) {
        let Some(destination) = self.current_location().as_path().map(ToOwned::to_owned) else {
            self.show_message(zsdesk_core::i18n::text("请选择当前文件夹中的压缩包").into());
            return;
        };
        let Some(browser) = self.browser.as_ref() else {
            return;
        };
        let paths = match unsafe { browser.current_view() }
            .and_then(|view| file_transfer::selected_paths(&view))
        {
            Ok(paths) if !paths.is_empty() && paths.iter().all(|path| archive::supported(path)) => {
                paths
            }
            Ok(_) => {
                self.show_message(zsdesk_core::i18n::text("请选择当前文件夹中的压缩包").into());
                return;
            }
            Err(error) => {
                self.show_message(error.to_string());
                return;
            }
        };
        let Some(identity) = self.identity_for_source(self.browser_source) else {
            return;
        };
        let owner = self.hwnd.0 as isize;
        let Some(operation) = Operation::acquire(&ACTIVE) else {
            self.show_message(zsdesk_core::i18n::text("已有四批解压任务正在进行，请稍后重试").into());
            return;
        };
        let started = std::thread::Builder::new()
            .name("zsdesk-extract".into())
            .spawn(move || {
                let _operation = operation;
                let count = paths.len();
                let mut completed = 0;
                let mut last_output = None;
                let mut first_error = None;
                for path in paths {
                    match archive::extract_here(&path, &destination) {
                        Ok(output) => {
                            completed += 1;
                            last_output = Some(output);
                        }
                        Err(error) => {
                            first_error
                                .get_or_insert_with(|| format!("{} — {error:#}", path.display()));
                        }
                    }
                }
                let message = match (first_error, last_output) {
                    (Some(error), _) => zsdesk_core::tr_format!(
                        "已解压 {completed}/{count} 个压缩包。{error}",
                        completed = completed,
                        count = count,
                        error = error
                    ),
                    (None, Some(output)) if count == 1 => {
                        zsdesk_core::tr_format!("已解压到 {}", output.display())
                    }
                    _ => zsdesk_core::tr_format!("已解压 {} 个压缩包", completed),
                };
                pane_input::post(
                    HWND(owner as *mut _),
                    identity,
                    pane_input::Intent::TransferMessage(message),
                );
            });
        match started {
            Ok(_) => self.show_message(zsdesk_core::i18n::text("正在解压…").into()),
            Err(error) => self.show_message(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_extraction_releases_capacity_after_a_job_finishes() {
        let active = AtomicUsize::new(0);
        let mut jobs: Vec<_> = (0..4).map(|_| Operation::acquire(&active).unwrap()).collect();
        assert!(Operation::acquire(&active).is_none());
        drop(jobs.pop());
        assert!(Operation::acquire(&active).is_some());
        drop(jobs);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }
}
