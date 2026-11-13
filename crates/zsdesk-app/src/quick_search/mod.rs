//! Shared hotkey/background lifecycle. Search lives in the file-manager window.
pub mod control;
mod resident;
use std::path::{Path,PathBuf};
use anyhow::{Context,Result};
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum Action {Open,Settings,Resident,Stop,Status,EnableAtLogin,Disable}

pub fn launch(profile: &Path, settings: bool) -> Result<()> {
    if control::signal(profile, if settings { "settings" } else { "open" })? { return Ok(()); }
    spawn_utility(profile,if settings {"--quick-search-settings"}else{"--quick-search"})
}

pub fn start_if_enabled(profile: &Path) -> Result<()> {
    if (!control::load(profile)?.preferences.enabled && !crate::explorer_forwarder::owns_integration(profile)) || control::running(profile) { return Ok(()); }
    spawn_utility(profile,"--background")
}

pub(crate) fn spawn_utility(profile:&Path,action:&str)->Result<()> {
    let arguments=vec![action.into(),"--profile-dir".into(),profile.parent().context("缺少配置目录")?.as_os_str().to_owned()];
    if crate::runtime::enqueue_local(profile,arguments)?{return Ok(());}
    use std::os::windows::ffi::OsStrExt;
    use windows::{core::{PCWSTR,PWSTR},Win32::{Foundation::CloseHandle,System::Threading::{CreateProcessW,CREATE_NO_WINDOW,STARTUPINFOW,PROCESS_INFORMATION}}};
    let executable=std::env::current_exe()?;
    let directory=profile.parent().context("缺少配置目录")?;
    let command=format!("{} {action} --profile-dir {}",control::quote_argument(&executable.to_string_lossy()),control::quote_argument(&directory.to_string_lossy()));
    let name:Vec<u16>=executable.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut command:Vec<u16>=command.encode_utf16().chain(Some(0)).collect();
    let startup=STARTUPINFOW{cb:std::mem::size_of::<STARTUPINFOW>() as u32,..Default::default()};
    let mut process=PROCESS_INFORMATION::default();
    // A resident process must not keep a launcher's redirected pipes or other
    // inheritable handles alive after that short-lived launcher has exited.
    unsafe {CreateProcessW(PCWSTR(name.as_ptr()),Some(PWSTR(command.as_mut_ptr())),None,None,false,CREATE_NO_WINDOW,None,PCWSTR::null(),&startup,&mut process)?;}
    let _=unsafe{CloseHandle(process.hThread)};let _=unsafe{CloseHandle(process.hProcess)};
    Ok(())
}

pub fn run(profile: PathBuf, action: Action) -> Result<()> {
    match action {
        Action::Status => { println!("{}", control::status(&profile)); return Ok(()); }
        Action::Stop => { println!("{}", serde_json::json!({"stop_requested":control::signal(&profile,"stop")?})); return Ok(()); }
        Action::EnableAtLogin => {
            let mut preferences = control::load(&profile)?.preferences;
            preferences.enabled = true; preferences.start_with_windows = true;
            control::apply(&profile, preferences)?;
            let _ = control::signal(&profile, "reload")?; start_if_enabled(&profile)?;
            println!("{}", control::status(&profile)); return Ok(());
        }
        Action::Disable => {
            let mut preferences = control::load(&profile)?.preferences;
            preferences.enabled = false; preferences.start_with_windows = false;
            control::apply(&profile, preferences)?; let _ = control::signal(&profile, if crate::explorer_forwarder::owns_integration(&profile){"reload"}else{"stop"})?;
            println!("{}", serde_json::json!({"enabled":false,"start_with_windows":false})); return Ok(());
        }
        _ => {}
    }
    let configuration = control::load(&profile)?;
    if action == Action::Resident && !configuration.preferences.enabled && !crate::explorer_forwarder::owns_integration(&profile) { return Ok(()); }
    let Some(instance) = control::Instance::acquire(&profile)? else {
        if action != Action::Resident { let _ = control::signal(&profile, if action == Action::Settings { "settings" } else { "open" })?; }
        return Ok(());
    };
    let _win_e_bridge = crate::win_e_bridge::Bridge::start(&profile).unwrap_or_else(|error| {
        crate::win_e_bridge::report_error(&profile, &format!("{error:#}")); None
    });
    let _explorer_forwarder=crate::explorer_forwarder::Bridge::start(&profile)?;
    resident::run(profile,action,&instance)
}
