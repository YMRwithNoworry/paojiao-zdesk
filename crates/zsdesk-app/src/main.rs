//! ZSDesk application entry.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod commands;
// Compiled in every edition; unreachable without the `desktop` feature and dropped by the linker.
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
mod desktop;
mod edition;
mod storage;
mod window;
mod instance;
mod reveal;
mod shell_window_registry;
mod context_preferences;
mod flyout_menu;
mod routing_control;
mod fast_search;
mod folder_size_data;
mod quick_search;
mod win_e_bridge;
mod explorer_forwarder;
mod file_dialog_takeover;
mod runtime;
mod context_menu_process;
mod utility_panel;
mod update_check;

/// Lets the installer wait for all application windows and desktop guards to
/// finish before replacing or uninstalling their files.
struct ApplicationLifetime(Option<windows::Win32::Foundation::HANDLE>);
impl ApplicationLifetime {
    fn new() -> Self {
        if app::is_import_cli() { return Self(None); }
        use windows::Win32::System::Threading::{CreateMutexExW, SYNCHRONIZATION_SYNCHRONIZE};
        let name: Vec<u16> = crate::edition::application_mutex().encode_utf16().chain(Some(0)).collect();
        Self(unsafe { CreateMutexExW(None, windows::core::PCWSTR(name.as_ptr()), 0, SYNCHRONIZATION_SYNCHRONIZE.0) }.ok())
    }
}
impl Drop for ApplicationLifetime {
    fn drop(&mut self) { if let Some(handle) = self.0.take() { let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) }; } }
}

fn main() {
    if let Some(result)=file_dialog_takeover::guard::run_if_requested(){if let Err(error)=result{eprintln!("{error:#}");std::process::exit(1);}return;}
    if let Some(result)=zsdesk_windows::shell::maybe_handle_packaged_state_worker(&std::env::args_os().skip(1).collect::<Vec<_>>()){
        if let Err(error)=result{eprintln!("{error}");std::process::exit(1);}return;
    }
    if let Some(result)=explorer_forwarder::run_guard_if_requested(){if let Err(error)=result{eprintln!("{error:#}");std::process::exit(1);}return;}
    if let Some(result)=context_menu_process::run_worker_if_requested(){if let Err(error)=result{eprintln!("{error:#}");std::process::exit(1);}return;}
    if let Some(result)=window::preview_process::run_if_requested(){
        if let Err(error)=result{eprintln!("{error:#}");std::process::exit(1);}return;
    }
    let _lifetime = ApplicationLifetime::new();
    if let Err(error)=app::run(){
        if app::is_import_cli() || app::is_background_mode() {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
        // Folders opened through system integration must never lose their opener.
        if let Some(target)=app::shell_open_target() {
            let _=app::open_original_explorer(target);
            return;
        }
        if app::is_win_e() && app::open_original_explorer("shell:MyComputerFolder".into()).is_ok(){return;}
        let message:Vec<u16>=format!("{error:#}").encode_utf16().chain(Some(0)).collect();
        unsafe {windows::Win32::UI::WindowsAndMessaging::MessageBoxW(None,windows::core::PCWSTR(message.as_ptr()),
            windows::core::w!("ZSDesk"),windows::Win32::UI::WindowsAndMessaging::MB_OK|windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR);}
        std::process::exit(1);
    }
}
