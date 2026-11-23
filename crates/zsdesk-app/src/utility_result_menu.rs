//! Quick Search binds its result row to the same isolated Shell menu host as the file window.
use super::*;
use crate::context_menu_process::{self, AsyncClient, MenuViewContext, Mode as MenuMode};
use zsdesk_core::model::explorer::ShellLocation;
use zsdesk_windows::shell::{
    file_dialogs, file_transfer,
    menu::{MenuInterceptions, MenuOutcome},
    pidl,
};
pub(super) const TIMER: usize = 0x5a86;
pub(super) const RENAME: u32 = WM_APP + 112;
pub(super) const BACKGROUND: u32 = WM_APP + 113;

#[derive(Clone)]
pub(super) struct Target {
    path: PathBuf,
    pidl: Vec<u8>,
    generation: u64,
    row: usize,
}
impl Target {
    fn at(&self, generation: u64, paths: &[PathBuf], selected: Option<usize>) -> bool {
        generation == self.generation
            && selected == Some(self.row)
            && paths.get(self.row) == Some(&self.path)
    }
}
pub(super) struct Session {
    client: AsyncClient,
    targets: Vec<Target>,
}
impl Panel {
    fn selected_target(&self, t: &Target) -> bool {
        if self.awaiting_page{return false;}
        report_list::selected_rows(self.list).len() == 1
            && t.at(
                self.generation,
                &self.paths,
                report_list::selected(self.list),
            )
            && unsafe { IsWindowVisible(self.hwnd) }.as_bool()
    }
    fn same_results(&self, targets: &[Target]) -> bool {
        if self.awaiting_page{return false;}
        let selected = report_list::selected_rows(self.list);
        selected.len() == targets.len()
            && !selected.is_empty()
            && targets
                .iter()
                .zip(selected)
                .all(|(target, row)| target.at(self.generation, &self.paths, Some(row)))
            && unsafe { IsWindowVisible(self.hwnd) }.as_bool()
    }
    pub(super) fn copy_result_selection(&self, paths_only: bool) -> Result<()> {
        if self.awaiting_page{return Ok(());}
        let paths: Vec<_> = report_list::selected_rows(self.list)
            .into_iter()
            .filter_map(|i| self.paths.get(i).cloned())
            .collect();
        if !paths.is_empty() {
            if paths_only {
                file_transfer::set_text(
                    self.hwnd,
                    &paths
                        .iter()
                        .map(|p| p.to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("\r\n"),
                )?;
            } else {
                file_transfer::copy_paths(self.hwnd, &paths, false)?;
            }
        }
        Ok(())
    }
    pub(super) fn queue_background_menu(&self, point: POINT) {
        let packed = ((point.x as u16 as u32) | ((point.y as u16 as u32) << 16)) as isize;
        let _ = unsafe { PostMessageW(Some(self.hwnd), BACKGROUND, WPARAM(0), LPARAM(packed)) };
    }
    pub(super) fn cancel_result_menu(&mut self) {
        self.result_menu = None;
        self.rename_target = None;
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER) };
    }
    pub(super) fn begin_result_menu(&mut self, position: LPARAM) -> Result<()> {
        if self.awaiting_page {return Ok(());}
        let keyboard = position.0 == -1;
        let (row, point) = if keyboard {
            let Some(row) = report_list::selected(self.list) else {
                let mut point = POINT { x: 12, y: 32 };
                if unsafe { ClientToScreen(self.list, &mut point) }.as_bool() {
                    self.queue_background_menu(point);
                }
                return Ok(());
            };
            let mut rect = RECT {
                left: LVIR_LABEL as i32,
                ..Default::default()
            };
            if unsafe {
                SendMessageW(
                    self.list,
                    LVM_GETITEMRECT,
                    Some(WPARAM(row)),
                    Some(LPARAM(&mut rect as *mut _ as isize)),
                )
            }
            .0 == 0
            {
                return Ok(());
            }
            let mut point = POINT {
                x: rect.left + 6,
                y: rect.bottom,
            };
            if !unsafe { ClientToScreen(self.list, &mut point) }.as_bool() {
                return Ok(());
            }
            (row, point)
        } else {
            let point = POINT {
                x: (position.0 as u16 as i16) as i32,
                y: ((position.0 >> 16) as u16 as i16) as i32,
            };
            let mut hit = LVHITTESTINFO {
                pt: point,
                ..Default::default()
            };
            if !unsafe { ScreenToClient(self.list, &mut hit.pt) }.as_bool() {
                return Ok(());
            }
            let raw = unsafe {
                SendMessageW(
                    self.list,
                    LVM_SUBITEMHITTEST,
                    None,
                    Some(LPARAM(&mut hit as *mut _ as isize)),
                )
            }
            .0;
            let Ok(row) = usize::try_from(raw) else {
                self.queue_background_menu(point);
                return Ok(());
            };
            if row >= self.paths.len() {
                return Ok(());
            }
            if !report_list::selected_rows(self.list).contains(&row) {
                report_list::set_selection(self.list, false);
                report_list::select(self.list, row);
            }
            (row, point)
        };
        if self
            .result_menu
            .as_ref()
            .is_some_and(|s| self.same_results(&s.targets))
        {
            return Ok(());
        }
        self.cancel_result_menu();
        let mut targets = Vec::new();
        for row in report_list::selected_rows(self.list) {
            let path = self.paths.get(row).context("搜索结果已改变")?.clone();
            let pidl = pidl::location_pidl(&ShellLocation::Path(path.clone()))
                .context("无法读取搜索结果的 Shell 标识")?;
            targets.push(Target {
                path,
                pidl,
                generation: self.generation,
                row,
            });
        }
        if !targets.iter().any(|t| t.row == row) {
            return Ok(());
        }
        let common = targets[0]
            .path
            .parent()
            .filter(|p| targets.iter().all(|t| t.path.parent() == Some(*p)))
            .map(Path::to_path_buf);
        let folder = common
            .as_ref()
            .and_then(|p| pidl::location_pidl(&ShellLocation::Path(p.clone())));
        let preferences = crate::context_preferences::current(&self.profile)
            .map(Ok)
            .unwrap_or_else(|| crate::storage::load(&self.profile).map(|s| s.ui.context_menu))?;
        let extended = !keyboard && unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        let context = MenuViewContext {
            folder,
            results: common.is_none(),
            keyboard,
            f10: keyboard,
        };
        let single = targets.len() == 1;
        let client = AsyncClient::spawn_with_context(
            self.hwnd,
            point,
            targets.iter().map(|t| t.pidl.clone()).collect(),
            common,
            crate::context_preferences::native(&preferences),
            MenuInterceptions {
                open: single,
                rename: single,
                file_actions: true,
            },
            MenuMode::More,
            extended,
            None,
            Some(context),
        )?;
        self.result_menu = Some(Session { client, targets });
        unsafe {
            SetTimer(Some(self.hwnd), TIMER, 50, None);
        }
        Ok(())
    }
    pub(super) fn poll_result_menu(&mut self) {
        let Some(mut session) = self.result_menu.take() else {
            let _ = unsafe { KillTimer(Some(self.hwnd), TIMER) };
            return;
        };
        let worker = session.client.pid();
        let result = session.client.poll(&mut |invoke| {
            let foreground = unsafe { GetForegroundWindow() };
            self.same_results(&session.targets)
                && (invoke
                    || foreground == self.hwnd
                    || context_menu_process::foreground_is_menu_worker(worker, self.hwnd))
        });
        let Some(result) = result else {
            self.result_menu = Some(session);
            return;
        };
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER) };
        let targets = session.targets;
        match result {
            Err(error) => self.status = context_menu_process::failure_message(&error),
            Ok(outcome) if self.same_results(&targets) => {
                if let Err(error) = self.finish_result_menu(outcome, targets) {
                    self.status = format!("{error:#}");
                }
            }
            _ => {}
        }
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }
    fn finish_result_menu(&mut self, outcome: MenuOutcome, mut targets: Vec<Target>) -> Result<()> {
        let pidls: Vec<_> = targets.iter().map(|t| t.pidl.clone()).collect();
        let paths: Vec<_> = targets.iter().map(|t| t.path.clone()).collect();
        let target = &targets[0];
        match outcome {
            MenuOutcome::Open | MenuOutcome::Explore => self.open_selected(false),
            MenuOutcome::Copy | MenuOutcome::Cut => {
                file_transfer::copy_paths(self.hwnd, &paths, outcome == MenuOutcome::Cut)?
            }
            MenuOutcome::Delete { permanent } => {
                file_transfer::delete_paths(self.hwnd, paths, permanent)?
            }
            MenuOutcome::Paste => {
                let destination = if targets.len() == 1 && target.path.is_dir() {
                    target.path.as_path()
                } else {
                    let parent = target.path.parent().context("无法确定粘贴目录")?;
                    anyhow::ensure!(
                        paths.iter().all(|path| path.parent() == Some(parent)),
                        "{}",
                        zsdesk_core::i18n::text("所选结果不在同一文件夹")
                    );
                    parent
                };
                if file_transfer::has_filesystem_clipboard() {
                    file_transfer::paste(self.hwnd, destination)?;
                } else {
                    let client = AsyncClient::spawn(
                        self.hwnd,
                        POINT::default(),
                        pidls.to_vec(),
                        target.path.parent().map(Path::to_path_buf),
                        zsdesk_windows::shell::menu::context_menu_options(),
                        Default::default(),
                        MenuMode::Paste,
                    )?;
                    self.result_menu = Some(Session { client, targets });
                    unsafe {
                        SetTimer(Some(self.hwnd), TIMER, 50, None);
                    }
                }
            }
            MenuOutcome::Share | MenuOutcome::ChooseApplication => {
                file_dialogs::show(self.hwnd, outcome, &pidls)?
            }
            MenuOutcome::PinInExplorer => {
                crate::explorer_forwarder::show_start_pin_target(&pidls, |_| {})?
            }
            MenuOutcome::Rename => {
                let _ = unsafe { SetFocus(Some(self.list)) };
                self.rename_target = targets.pop();
                let _ = unsafe { PostMessageW(Some(self.hwnd), RENAME, WPARAM(0), LPARAM(0)) };
            }
            MenuOutcome::ShellInvoked | MenuOutcome::NativeHandled => self.search(),
            MenuOutcome::Dismissed => {}
            _ => {}
        }
        Ok(())
    }
    pub(super) fn edit_row(&self) -> Option<usize> {
        if self.awaiting_page{return None;}
        self.rename_target
            .as_ref()
            .filter(|t| self.selected_target(t))
            .map(|t| t.row)
    }
    pub(super) fn finish_rename(&mut self, item: &NMLVDISPINFOW) -> bool {
        let Some(target) = self.rename_target.take() else {
            return false;
        };
        if item.item.pszText.is_null()
            || item.item.iItem != target.row as i32
            || !self.selected_target(&target)
        {
            return false;
        }
        let Ok(name) = (unsafe { item.item.pszText.to_string() }) else {
            return false;
        };
        if name.is_empty() || name.contains(['\\', '/', ':']) || name == "." || name == ".." {
            return false;
        }
        if target
            .path
            .file_name()
            .is_some_and(|old| old == name.as_str())
        {
            return false;
        }
        match zsdesk_windows::desktop::rename(self.hwnd, &target.pidl, &name) {
            Ok(()) => {
                if let Some(p) = self.paths.get_mut(target.row) {
                    p.set_file_name(&name);
                    self.search_view.table.rename(target.row,p);
                    unsafe{SendMessageW(self.list,LVM_REDRAWITEMS,Some(WPARAM(target.row)),Some(LPARAM(target.row as isize)));}
                }
                true
            }
            Err(error) => {
                self.status = error.to_string();
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_file_selection_constructs_a_shell_menu_without_invoking_commands() -> Result<()> {
        use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
        use zsdesk_windows::shell::menu::ShellMenu;
        unsafe {
            OleInitialize(None)?;
        }
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                unsafe {
                    OleUninitialize();
                }
            }
        }
        let _apartment = Apartment;
        let dir = std::env::temp_dir().join(format!("zsdesk-results-menu-{}", std::process::id()));
        std::fs::create_dir(&dir)?;
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                for name in ["one.txt", "two.txt"] {
                    let _ = std::fs::remove_file(self.0.join(name));
                }
                let _ = std::fs::remove_dir(&self.0);
            }
        }
        let _fixture = Fixture(dir.clone());
        let mut items = Vec::new();
        for name in ["one.txt", "two.txt"] {
            let path = dir.join(name);
            std::fs::write(&path, b"fixture")?;
            items.push(pidl::location_pidl(&ShellLocation::Path(path)).context("file identity")?);
        }
        for len in [1, 2] {
            let menu = ShellMenu::for_items(HWND::default(), &items[..len])?;
            menu.populate_for_invoke(false)?;
            assert!(unsafe { GetMenuItemCount(Some(menu.hmenu())) } > 0);
        }
        Ok(())
    }
    #[test]
    fn menu_validation_follows_the_exact_result_and_query() {
        let t = Target {
            path: PathBuf::from(r"C:\parent\one.pdf"),
            pidl: vec![],
            generation: 17,
            row: 1,
        };
        let paths = vec![PathBuf::from(r"C:\other\one.pdf"), t.path.clone()];
        assert!(t.at(17, &paths, Some(1)));
        assert!(!t.at(18, &paths, Some(1)));
        assert!(!t.at(17, &paths, Some(0)));
        assert!(!t.at(17, &[paths[1].clone(), paths[0].clone()], Some(1)));
    }
}
