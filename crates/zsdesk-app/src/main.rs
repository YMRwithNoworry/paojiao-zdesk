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
mod context_preferences;
mod routing_control;
mod fast_search;
mod win_e_agent;

/// Lets the installer wait for all application windows and desktop guards to
/// finish before replacing or uninstalling their files.
struct ApplicationLifetime(Option<windows::Win32::Foundation::HANDLE>);
impl ApplicationLifetime {
    fn new() -> Self {
        if app::is_import_cli() { return Self(None); }
        use windows::Win32::System::Threading::{CreateMutexExW, SYNCHRONIZATION_SYNCHRONIZE};
        Self(unsafe { CreateMutexExW(None, windows::core::w!("Local\\ZSDesk.Native.Application"), 0, SYNCHRONIZATION_SYNCHRONIZE.0) }.ok())
    }
}
impl Drop for ApplicationLifetime {
    fn drop(&mut self) { if let Some(handle) = self.0.take() { let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) }; } }
}

fn main() {
    let _lifetime = ApplicationLifetime::new();
    if let Err(error)=app::run(){
        if app::is_import_cli() {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
        // Folders opened through system integration must never lose their opener.
        if let Some(target)=app::shell_open_target() {
            let _=std::process::Command::new("explorer.exe").arg(target).spawn();
            return;
        }
        let message:Vec<u16>=format!("{error:#}").encode_utf16().chain(Some(0)).collect();
        unsafe {windows::Win32::UI::WindowsAndMessaging::MessageBoxW(None,windows::core::PCWSTR(message.as_ptr()),
            windows::core::w!("ZSDesk"),windows::Win32::UI::WindowsAndMessaging::MB_OK|windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR);}
        std::process::exit(1);
    }
}
