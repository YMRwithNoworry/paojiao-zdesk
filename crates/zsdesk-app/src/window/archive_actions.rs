use super::{pane_input, WindowState};
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::Foundation::HWND;
use zsdesk_windows::{archive, shell::file_transfer};

static ACTIVE: AtomicUsize = AtomicUsize::new(0);
pub(crate) fn pending_operations() -> usize {
    ACTIVE.load(Ordering::Acquire)
}

struct Operation;
impl Drop for Operation {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
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
        ACTIVE.fetch_add(1, Ordering::AcqRel);
        let operation = Operation;
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
