//! Prints how the desktop Shell cooperation layer sees this machine.
fn main() {
    unsafe { let _ = windows::Win32::System::Ole::OleInitialize(None); }
    let view = zsdesk_windows::desktop::desktop_view();
    println!("desktop_view: {}", if view.is_ok() { "ok".to_string() } else { format!("{:?}", view.as_ref().err()) });
    if let Ok(view) = &view {
        let flags = unsafe { view.GetCurrentFolderFlags() };
        println!("folder flags: {flags:x?}");
    }
    println!("defview: {:?}", zsdesk_windows::desktop::desktop_defview());
    println!("icons_visible: {}", zsdesk_windows::desktop::icons_visible());
    let items = zsdesk_windows::desktop::enumerate_desktop();
    println!("desktop items: {}", items.len());
    for e in items.iter().take(6) { println!("  {} | {} | folder={} link={} fs={}", e.name, e.key, e.is_folder, e.is_link, e.is_file_system); }
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--parse") {
        let name = zsdesk_core::model::explorer::ShellLocation::Shell { pidl:None, parsing_name: args[i + 1].clone(), title: String::new() };
        match unsafe { zsdesk_windows::shell::shell_item(&name) } {
            Ok(item) => println!("parse ok: {:?}", unsafe { zsdesk_windows::shell::location_from_shell_item(&item) }),
            Err(e) => println!("parse failed: {e}"),
        }
        return;
    }
    if args.iter().any(|a| a == "--integration") {
        use zsdesk_windows::integration::{apply, current, registered_command, restore, Integration};
        let backup = std::env::temp_dir().join("zsdesk-integration-probe.json");
        let exe = std::env::current_exe().unwrap();
        println!("before: {:?}", current());
        apply(Integration::Default, &exe, &backup).unwrap();
        println!("applied: {:?} command={:?}", current(), registered_command());
        apply(Integration::ContextMenu, &exe, &backup).unwrap();
        println!("context: {:?}", current());
        restore(&backup).unwrap();
        println!("restored: {:?} command={:?} backup_left={}", current(), registered_command(), backup.exists());
        return;
    }
    if std::env::args().any(|a| a == "--toggle") {
        let now = zsdesk_windows::desktop::icons_visible();
        let after = zsdesk_windows::desktop::set_icons_visible(!now);
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("toggle: {now} -> reported {after}, rechecked {}", zsdesk_windows::desktop::icons_visible());
        let back = zsdesk_windows::desktop::set_icons_visible(now);
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("restore: reported {back}, rechecked {}", zsdesk_windows::desktop::icons_visible());
    }
}
