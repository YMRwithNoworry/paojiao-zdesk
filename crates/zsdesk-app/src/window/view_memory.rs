//! Stable per-profile names for Shell-managed folder view preferences.

use std::path::Path;
use sha2::{Digest, Sha256};

/// The Shell keys the stored layout by this application name and the folder's
/// identity. Tabs keep their own selection/layout snapshots independently.
/// Neither the executable path nor the program version belongs in this name.
pub(super) fn property_bag_name(profile: &Path) -> String {
    let path = profile.to_string_lossy().replace('/', "\\").to_lowercase();
    let digest = Sha256::digest(path.as_bytes());
    format!("ZSDesk.FolderViews.v1.{digest:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore="Read-only native Shell ordering comparison on isolated fixture folders; requires ZSDESK_NATIVE_TEST_OUTPUT"]
    fn native_filename_order_matches_windows_shell()->anyhow::Result<()>{
        use anyhow::{Context,ensure};
        use windows::{core::w,Win32::{Foundation::RECT,System::Ole::{OleInitialize,OleUninitialize},UI::WindowsAndMessaging::*}};
        use zsdesk_windows::shell::{ExplorerBrowserHost,Panes,commands::Column};
        let output=std::path::PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output required")?);
        ensure!(output.is_absolute(),"absolute fixture root required");let root=output.join("system-sort-fixtures");std::fs::create_dir_all(&root)?;
        let names=["文件10","文件2","阿","波","赵","张","A10","A2","ä","z","_one","-two"];
        for name in names{std::fs::create_dir_all(root.join(name))?;}
        unsafe{OleInitialize(None)}?;struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
        let hwnd=unsafe{CreateWindowExW(WS_EX_NOACTIVATE,w!("STATIC"),w!("Sort fixture"),WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,
            -30000,-30000,900,650,None,None,None,None)}?;
        struct Window(windows::Win32::Foundation::HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};pump(30);}}let _window=Window(hwnd);
        let browser=unsafe{ExplorerBrowserHost::create_transient(hwnd,RECT{left:0,top:0,right:880,bottom:610},false,Panes::default())}?;
        browse(&browser,&root)?;browser.set_group(None)?;browser.set_sort(Column::Name,true)?;pump(200);
        let actual=browser.item_paths_limit(64).into_iter().filter_map(|path|std::path::Path::new(&path).file_name().map(|name|name.to_string_lossy().into_owned())).collect::<Vec<_>>();
        let mut expected=names.iter().map(|name|name.to_string()).collect::<Vec<_>>();expected.sort_by(|a,b|zsdesk_core::model::view::natural_cmp(a,b));
        ensure!(actual==expected,"native collation differs: Shell={actual:?}, model={expected:?}");
        std::fs::write(output.join("system-sort.current.json"),serde_json::to_vec_pretty(&serde_json::json!({"state":"passed","shell":actual,"model":expected,"user_settings_changed":false}))?)?;
        Ok(())
    }

    #[test]
    fn folder_preferences_share_a_profile_but_not_a_different_profile() {
        let root = std::env::temp_dir().join("zsdesk-view-memory-identity");
        let profile = root.join("配置").join("explorer-session.json");
        let spelling = profile.to_string_lossy().replace('\\', "/").to_ascii_uppercase();
        let first = property_bag_name(&profile);
        assert_eq!(first, property_bag_name(Path::new(&spelling)));
        assert_ne!(first, property_bag_name(&root.join("second-profile").join("explorer-session.json")));
        assert!(first.starts_with("ZSDesk.FolderViews.v1."));
        assert!(!first.contains("配置") && !first.contains('\\') && !first.contains('/'));
    }

    #[test]
    #[ignore = "Isolated Shell property-bag persistence; requires ZSDESK_NATIVE_TEST_OUTPUT, reuses registered fixture folders and a stable test-only bag"]
    fn native_folder_layouts_survive_navigation_and_browser_recreation() -> anyhow::Result<()> {
        use anyhow::{Context, ensure};
        use std::{fs, path::PathBuf, os::windows::fs::MetadataExt};
        use windows::{core::w, Win32::{Foundation::RECT,
            System::Ole::{OleInitialize, OleUninitialize}, UI::WindowsAndMessaging::*}};
        use zsdesk_windows::shell::{ExplorerBrowserHost, Panes};

        let output = PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output required")?);
        ensure!(output.is_absolute(), "output must be absolute");
        fs::create_dir_all(&output)?;
        // Keep both the PIDL locations and bag name stable across runs. Shell
        // owns registry persistence; tests never enumerate or delete real bags.
        let root = output.join("folder-view-memory-fixtures");
        fs::create_dir_all(&root)?;
        ensure!(fs::symlink_metadata(&root)?.file_attributes() & 0x400 == 0, "fixture root must be a real directory");
        let first = root.join("A");
        let second = root.join("B");
        for path in [&first, &second] {
            fs::create_dir_all(path)?;
            ensure!(fs::symlink_metadata(path)?.file_attributes() & 0x400 == 0, "fixture folder must be real");
            for (name, bytes) in [("first.txt", b"one"), ("second.txt", b"two")] {
                let file = path.join(name);
                if file.try_exists()? {
                    let meta = fs::symlink_metadata(&file)?;
                    ensure!(meta.is_file() && meta.file_attributes() & 0x400 == 0 && fs::read(&file)?.as_slice() == bytes.as_slice(),
                        "fixture contents changed: {}", file.display());
                } else { fs::write(file, bytes)?; }
            }
        }
        // This namespace never matches the production profile bag. Native
        // persistence is exercised only for these registered fixture identities.
        let bag = property_bag_name(&root).replacen("ZSDesk.FolderViews.", "ZSDesk.NativeTest.FolderViews.", 1);
        struct Record { path: PathBuf, value: serde_json::Value, finished: bool }
        impl Drop for Record {
            fn drop(&mut self) {
                if !self.finished {
                    self.value["state"] = "failed".into();
                    if let Ok(bytes) = serde_json::to_vec_pretty(&self.value) { let _ = fs::write(&self.path, bytes); }
                }
            }
        }
        let mut record = Record { path: output.join("folder-view-memory.current.json"), finished: false,
            value: serde_json::json!({"state":"running", "phase":"setup", "test_bag":bag, "fixture_root":root,
                "fixture_files":["A/first.txt", "A/second.txt", "B/first.txt", "B/second.txt"],
                "namespace_policy":"stable-per-fixture-root", "fixture_bytes":12, "windows_hidden":true}) };
        fs::write(&record.path, serde_json::to_vec_pretty(&record.value)?)?;
        unsafe { OleInitialize(None)?; }
        struct Apartment;
        impl Drop for Apartment { fn drop(&mut self) { unsafe { OleUninitialize(); } } }
        let _apartment = Apartment;
        let hwnd = unsafe { CreateWindowExW(WS_EX_NOACTIVATE, w!("STATIC"), w!("Folder view persistence"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, -30000, -30000, 900, 650, None, None, None, None)? };
        struct Window(windows::Win32::Foundation::HWND);
        impl Drop for Window {
            fn drop(&mut self) { let _ = unsafe { DestroyWindow(self.0) }; pump(40); }
        }
        let _window = Window(hwnd);
        ensure!(!unsafe { IsWindowVisible(hwnd) }.as_bool(), "test window must stay hidden");
        let rectangle = RECT { left: 0, top: 0, right: 880, bottom: 610 };
        let create = |transient: bool| -> anyhow::Result<ExplorerBrowserHost> {
            let browser = unsafe {
                if transient { ExplorerBrowserHost::create_transient(hwnd, rectangle, false, Panes::default()) }
                else { ExplorerBrowserHost::create(hwnd, rectangle, false, Panes::default()) }
            }?;
            browser.set_view_state_name(&bag)?;
            Ok(browser)
        };

        record.value["phase"] = "create-browser".into();
        let browser = create(false)?;
        record.value["phase"] = "browse-folder-a".into();
        browse(&browser, &first)?;
        record.value["phase"] = "configure-folder-a".into();
        let a = configure(&browser, false)?;
        record.value["phase"] = "browse-folder-b".into();
        browse(&browser, &second)?;
        record.value["phase"] = "configure-folder-b-date-sort".into();
        let b = configure(&browser, true)?;
        ensure!((a.mode, a.icon_size, &a.sort_columns) != (b.mode, b.icon_size, &b.sort_columns), "fixtures need different views");
        record.value["phase"] = "restore-folder-a".into();
        browse(&browser, &first)?;
        assert_layout(&browser, &a, "A after navigating through B")?;
        drop(browser);
        pump(100);

        record.value["phase"] = "recreate-browser".into();
        let browser = create(false)?;
        record.value["phase"] = "restore-folder-b-date-sort".into();
        browse(&browser, &second)?;
        assert_layout(&browser, &b, "B after browser recreation")?;
        record.value["phase"] = "restore-folder-a-after-recreation".into();
        browse(&browser, &first)?;
        assert_layout(&browser, &a, "A after browser recreation")?;
        drop(browser);
        pump(100);

        record.value["phase"] = "check-transient-isolation".into();
        let transient = create(true)?;
        browse(&transient, &first)?;
        let _ = configure(&transient, true)?;
        drop(transient);
        pump(100);
        record.value["phase"] = "verify-final-view".into();
        let browser = create(false)?;
        browse(&browser, &first)?;
        assert_layout(&browser, &a, "transient host must not replace A preferences")?;
        drop(browser);
        pump(100);
        ensure!(!unsafe { IsWindowVisible(hwnd) }.as_bool(), "test window became visible");
        record.value["verification"] = serde_json::json!({ "folders": 2,
            "navigation": true, "browser_recreation": true, "transient_isolation": true,
            "view_mode_sort_group_columns_preserved": true,
            "a": a, "b": b
        });
        record.value["state"] = "passed".into();
        fs::write(&record.path, serde_json::to_vec_pretty(&record.value)?)?;
        record.finished = true;
        Ok(())
    }

    fn pump(milliseconds: u64) -> Vec<zsdesk_core::model::explorer::ShellLocation> {
        use std::{thread, time::{Duration, Instant}};
        use windows::Win32::UI::WindowsAndMessaging::*;
        use zsdesk_core::model::explorer::ShellLocation;
        use zsdesk_windows::shell::{acknowledge_selection_changed, WM_SHELL_NAVIGATED,
            WM_SHELL_FOLDER_INVOKED, WM_SHELL_SELECTION_CHANGED};
        let end = Instant::now() + Duration::from_millis(milliseconds);
        let mut locations = Vec::new();
        while Instant::now() < end {
            let mut message = MSG::default();
            while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                match message.message {
                    WM_SHELL_NAVIGATED | WM_SHELL_FOLDER_INVOKED if message.lParam.0 != 0 => {
                        let location = unsafe { Box::from_raw(message.lParam.0 as *mut ShellLocation) };
                        if message.message == WM_SHELL_NAVIGATED { locations.push(*location); }
                    }
                    WM_SHELL_SELECTION_CHANGED => unsafe { acknowledge_selection_changed(message.lParam.0); },
                    WM_QUIT => {},
                    _ => unsafe { let _ = TranslateMessage(&message); DispatchMessageW(&message); },
                }
            }
            thread::sleep(Duration::from_millis(4));
        }
        locations
    }

    fn browse(browser: &zsdesk_windows::shell::ExplorerBrowserHost, path: &Path) -> anyhow::Result<()> {
        use anyhow::{ensure, Context};
        use std::time::{Duration, Instant};
        use zsdesk_core::model::explorer::ShellLocation;
        pump(20);
        let expected = std::fs::canonicalize(path)?;
        let shell_path = path.to_string_lossy().replace('/', "\\");
        let location = ShellLocation::Path(shell_path.into());
        unsafe { zsdesk_windows::shell::shell_item(&location) }.context("parse fixture folder as Shell item")?;
        unsafe { browser.navigate(&location) }.context("navigate ExplorerBrowser to fixture folder")?;
        let start = Instant::now();
        loop {
            if pump(40).iter().filter_map(ShellLocation::as_path).any(|current|
                std::fs::canonicalize(current).is_ok_and(|current| current == expected)) {
                pump(160);
                return Ok(());
            }
            ensure!(start.elapsed() < Duration::from_secs(12), "navigation did not complete: {}", path.display());
        }
    }

    fn configure(browser: &zsdesk_windows::shell::ExplorerBrowserHost, alternate: bool)
        -> anyhow::Result<zsdesk_core::model::explorer::ShellViewState> {
        use anyhow::Context;
        use windows::{core::Interface, Win32::{Foundation::PROPERTYKEY,
            UI::Shell::{IFolderView2, IColumnManager, CM_COLUMNINFO, CM_MASK_WIDTH, SORTCOLUMN, SORT_ASCENDING, SORT_DESCENDING}}};
        use zsdesk_windows::shell::commands::{PKEY_NAME, PKEY_SIZE, PKEY_MODIFIED, PKEY_TYPE};
        browser.set_view_mode(4, 16)?;
        let shell = unsafe { browser.current_view()? };
        let view: IFolderView2 = shell.cast()?;
        let columns: IColumnManager = shell.cast()?;
        let keys = if alternate { [PKEY_NAME, PKEY_TYPE, PKEY_MODIFIED, PKEY_SIZE] }
            else { [PKEY_NAME, PKEY_SIZE, PKEY_MODIFIED, PKEY_TYPE] };
        unsafe { columns.SetColumns(&keys)?; }
        for (key, width) in keys.iter().zip(if alternate { [185, 135, 215, 105] } else { [275, 125, 175, 145] }) {
            let info = CM_COLUMNINFO { cbSize: std::mem::size_of::<CM_COLUMNINFO>() as u32,
                dwMask: CM_MASK_WIDTH.0 as u32, uWidth: width, ..Default::default() };
            unsafe { columns.SetColumnInfo(key, &info)?; }
        }
        let group = if alternate { PROPERTYKEY::default() } else { PKEY_TYPE };
        unsafe {
            view.SetGroupBy(&group, true)?;
            view.SetSortColumns(&[SORTCOLUMN { propkey: if alternate { PKEY_MODIFIED } else { PKEY_NAME },
                direction: if alternate { SORT_ASCENDING } else { SORT_DESCENDING } }])?;
        }
        if alternate { browser.set_view_mode(1, 48)?; }
        pump(160);
        unsafe { browser.capture_view() }.context("configured layout")
    }

    fn assert_layout(browser: &zsdesk_windows::shell::ExplorerBrowserHost,
        expected: &zsdesk_core::model::explorer::ShellViewState, phase: &str) -> anyhow::Result<()> {
        use anyhow::{Context, ensure};
        let actual = unsafe { browser.capture_view() }.context("restored layout")?;
        ensure!((actual.mode, actual.icon_size) == (expected.mode, expected.icon_size),
            "{phase}: view mode/icon size differ: {actual:?} vs {expected:?}");
        ensure!(actual.sort_columns == expected.sort_columns, "{phase}: sort differs");
        ensure!(actual.grouping == expected.grouping, "{phase}: grouping differs");
        ensure!(actual.columns == expected.columns && actual.column_dpi == expected.column_dpi,
            "{phase}: visible columns/order/width differ: {:?} vs {:?}", actual.columns, expected.columns);
        Ok(())
    }
}
