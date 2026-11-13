//! Per-profile background lifecycle and current-user logon registration.
use std::{fs, path::{Path, PathBuf}, sync::{Arc, atomic::{AtomicBool, Ordering}}, thread::{self, JoinHandle}};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows::{core::{w, PCWSTR}, Win32::{Foundation::{CloseHandle, GetLastError, HANDLE, ERROR_ALREADY_EXISTS},
    System::{Registry::{RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW,
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ},
        Threading::{CreateEventW, CreateMutexW, OpenEventW, OpenMutexW, SetEvent, WaitForMultipleObjects,
            EVENT_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE, INFINITE}}}};
use zsdesk_core::model::explorer::QuickSearchPreferences;
use zsui::{ZsAccelerator, ZsAcceleratorKey};

const RUN: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const ACTIONS: &[&str] = &["open", "settings", "rebuild", "reload", "stop"];
pub fn action_for_index(index:usize)->Option<&'static str>{ACTIONS.get(index).copied()}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Configuration {
    #[serde(flatten)]
    pub preferences: QuickSearchPreferences,
    pub startup_command: Option<String>,
}

pub fn key(profile: &Path) -> String {
    let path = profile.to_string_lossy().replace('/', "\\").to_lowercase();
    format!("{:x}", Sha256::digest(path.as_bytes()))
}
fn config_path(profile: &Path) -> PathBuf { profile.with_file_name("quick-search.json") }
fn wide(text: &str) -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() }
fn name(profile: &Path, action: &str) -> Vec<u16> { wide(&format!("Local\\ZSDesk.QuickSearch.{}.{}", key(profile), action)) }

pub fn load(profile: &Path) -> Result<Configuration> {
    let path = config_path(profile);
    if !path.try_exists()? { return Ok(Configuration::default()); }
    if fs::metadata(&path)?.len() > 65536 { bail!("后台搜索配置过大"); }
    serde_json::from_slice(&fs::read(path)?).context("后台搜索配置格式无效")
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    fs::create_dir_all(path.parent().context("配置目录缺失")?)?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, bytes)?;
    let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)?; }
    Ok(())
}

pub fn quote_argument(value: &str) -> String {
    let mut result = String::from("\""); let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' { slashes += 1; continue; }
        result.extend(std::iter::repeat_n('\\', if ch == '"' { slashes * 2 + 1 } else { slashes }));
        result.push(ch); slashes = 0;
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2)); result.push('"'); result
}

fn startup_name(profile: &Path) -> String { format!("ZSDesk.QuickSearch.{}", &key(profile)[..24]) }
fn startup_command(profile: &Path, executable: &Path) -> String {
    format!("{} --background --profile-dir {}", quote_argument(&executable.to_string_lossy()),
        quote_argument(&profile.parent().unwrap_or(profile).to_string_lossy()))
}

fn read_startup(profile: &Path) -> Result<Option<String>> {
    let name = wide(&startup_name(profile)); let mut bytes = 0;
    let status = unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN, PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, None, Some(&mut bytes)) };
    if status.0 == 2 { return Ok(None); }
    status.ok()?;
    if bytes > 65536 { bail!("登录启动项过大"); }
    let mut value = vec![0u16; bytes as usize / 2 + 1];
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN, PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, Some(value.as_mut_ptr().cast()), Some(&mut bytes)).ok()?; }
    Ok(Some(String::from_utf16_lossy(&value[..value.iter().position(|unit| *unit == 0).unwrap_or(value.len())])))
}

fn write_startup(profile: &Path, value: Option<&str>) -> Result<()> {
    let mut key = HKEY::default();
    unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, RUN, None, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut key, None).ok()?; }
    let name = wide(&startup_name(profile));
    let result = match value {
        Some(value) => { let value = wide(value); let bytes = unsafe { std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len() * 2) };
            unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)) }.ok() },
        None => { let status = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) }; if status.0 == 2 { Ok(()) } else { status.ok() } },
    };
    let _ = unsafe { RegCloseKey(key) }; result?; Ok(())
}

pub fn validate_hotkey(value: &str) -> Result<String> {
    if value.len() > 128 { bail!("快捷键过长"); }
    let chord: ZsAccelerator = value.trim().parse().map_err(|error| anyhow::anyhow!("{error}"))?;
    if chord.key() == ZsAcceleratorKey::Function(12) { bail!("F12 为系统调试器保留，请选择其它快捷键"); }
    if !chord.uses_primary() && !chord.uses_alt() && !chord.uses_super() && !matches!(chord.key(), ZsAcceleratorKey::Function(_)) {
        bail!("全局快捷键需包含 Ctrl、Alt、Win，或使用功能键");
    }
    Ok(chord.to_string().replace("Primary", "Ctrl").replace("Super", "Win"))
}

pub fn apply(profile: &Path, mut preferences: QuickSearchPreferences) -> Result<Configuration> {
    preferences.hotkey = validate_hotkey(&preferences.hotkey)?;
    if !preferences.enabled { preferences.start_with_windows = false; }
    let previous = load(profile)?; let before = read_startup(profile)?;
    let executable = std::env::current_exe()?;
    let desired = preferences.start_with_windows.then(|| startup_command(profile, &executable));
    if before.is_some() && before != previous.startup_command && before != desired {
        bail!("同名登录启动项已由其它设置修改，现有值已保留");
    }
    let next = Configuration { preferences, startup_command: desired.clone() };
    write_startup(profile, desired.as_deref())?;
    if let Err(error) = atomic_write(&config_path(profile), &serde_json::to_vec_pretty(&next)?) {
        if read_startup(profile).ok().flatten() == desired { let _ = write_startup(profile, before.as_deref()); }
        return Err(error);
    }
    Ok(next)
}

pub fn uninstall_owned(profile: &Path, executable: &Path) -> Result<()> {
    let own = startup_command(profile, executable);
    if read_startup(profile)?.as_deref() == Some(&own) { write_startup(profile, None)?; }
    main_window_startup::uninstall_owned(profile, executable)?;
    // Personal preferences are retained. Another installation's entry is untouched.
    Ok(())
}

/// A separate login entry opens the file manager without enabling its global
/// search shortcut. The command identifies both this executable and its profile.
pub mod main_window_startup {
    use super::{key, quote_argument};
    use anyhow::{Context, Result};
    use std::path::Path;
    use zsdesk_windows::desktop::{autostart_command, autostart_owned, set_autostart};

    fn value_name(profile: &Path) -> String {
        format!("ZSDesk.Window.{}", &key(profile)[..24])
    }

    fn command(profile: &Path, executable: &Path) -> Result<String> {
        let directory = profile.parent().context("缺少主窗口配置目录")?;
        anyhow::ensure!(executable.is_absolute() && directory.is_absolute(), "登录启动需要绝对程序路径和配置目录");
        Ok(format!("{} --profile-dir {}", quote_argument(&executable.to_string_lossy()),
            quote_argument(&directory.to_string_lossy())))
    }

    pub fn enabled(profile: &Path) -> Result<bool> {
        let expected = command(profile, &std::env::current_exe()?)?;
        Ok(autostart_command(&value_name(profile))?.as_deref()
            .is_some_and(|current| autostart_owned(current, &expected)))
    }

    pub fn set_enabled(profile: &Path, enabled: bool) -> Result<()> {
        let expected = command(profile, &std::env::current_exe()?)?;
        set_autostart(&value_name(profile), &expected, enabled)?;
        Ok(())
    }

    pub(super) fn uninstall_owned(profile: &Path, executable: &Path) -> Result<()> {
        let expected = command(profile, executable)?;
        let name = value_name(profile);
        if autostart_command(&name)?.as_deref().is_some_and(|current| autostart_owned(current, &expected)) {
            set_autostart(&name, &expected, false)?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn login_command_binds_executable_and_profile_without_background_mode() {
            let profile = Path::new(r"C:\Profiles\中文 Main\explorer-session.json");
            let executable = Path::new(r"C:\Program Files\ZSDesk\ZSDesk.exe");
            let own = command(profile, executable).unwrap();
            assert_eq!(own, r#""C:\Program Files\ZSDesk\ZSDesk.exe" --profile-dir "C:\Profiles\中文 Main""#);
            for other in [command(Path::new(r"C:\Profiles\Other\explorer-session.json"), executable).unwrap(),
                command(profile, Path::new(r"C:\Other\ZSDesk.exe")).unwrap(), format!("{own} --background")] {
                assert!(!autostart_owned(&other, &own));
            }
            assert!(command(Path::new("explorer-session.json"), executable).is_err());
            assert!(command(profile, Path::new("ZSDesk.exe")).is_err());
        }

        #[test]
        fn login_entry_is_separate_from_background_and_other_profiles() {
            let profile = Path::new(r"C:\Profiles\Main\explorer-session.json");
            assert_ne!(value_name(profile), super::super::startup_name(profile));
            assert_ne!(value_name(profile), value_name(Path::new(r"C:\Profiles\Theme\explorer-session.json")));
            assert_eq!(value_name(profile), value_name(Path::new(r"c:/profiles/main/explorer-session.json")));
        }

        #[test]
        #[ignore="Owned temporary HKCU login entries; requires isolated ZSDESK_NATIVE_TEST_OUTPUT"]
        fn native_login_entries_roundtrip() -> Result<()> {
            use super::super::{apply,load,read_startup,uninstall_owned};
            use zsdesk_core::model::explorer::QuickSearchPreferences;
            let output=std::path::PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output")?).join("startup");
            anyhow::ensure!(output.is_absolute(),"absolute output required");
            std::fs::create_dir_all(&output)?;
            let profile=output.join("explorer-session.json");let executable=std::env::current_exe()?;
            anyhow::ensure!(autostart_command(&value_name(&profile))?.is_none()&&read_startup(&profile)?.is_none(),"test login entry already exists; preserve it");
            struct Cleanup(std::path::PathBuf,std::path::PathBuf);
            impl Drop for Cleanup{fn drop(&mut self){let _=uninstall_owned(&self.0,&self.1);}}
            let cleanup=Cleanup(profile.clone(),executable.clone());
            set_enabled(&profile,true)?;
            anyhow::ensure!(enabled(&profile)?&&read_startup(&profile)?.is_none(),"main login changed background entry");
            let preferences=QuickSearchPreferences{enabled:true,start_with_windows:true,..Default::default()};
            apply(&profile,preferences)?;
            anyhow::ensure!(enabled(&profile)?&&read_startup(&profile)?.as_deref()==Some(&super::super::startup_command(&profile,&executable)),"background login was not registered exactly");
            set_enabled(&profile,false)?;
            anyhow::ensure!(!enabled(&profile)?&&read_startup(&profile)?.is_some(),"main disable changed background entry");
            let mut preferences=load(&profile)?.preferences;preferences.enabled=false;
            apply(&profile,preferences)?;
            anyhow::ensure!(read_startup(&profile)?.is_none()&&!load(&profile)?.preferences.start_with_windows,"background disable left a login entry");
            drop(cleanup);
            anyhow::ensure!(autostart_command(&value_name(&profile))?.is_none()&&read_startup(&profile)?.is_none(),"owned login entries not removed");
            let report=serde_json::json!({"state":"passed","main_and_background_independent":true,"registered_commands_verified":true,"owned_entries_removed":true,"started_at_login":false});
            std::fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");Ok(())
        }
    }
}

struct Kernel(HANDLE);
// These handles are kernel events/mutexes, never thread-affine HWND/COM objects.
unsafe impl Send for Kernel {}
unsafe impl Sync for Kernel {}
impl Drop for Kernel { fn drop(&mut self) { let _ = unsafe { CloseHandle(self.0) }; } }

pub fn signal(profile: &Path, action: &str) -> Result<bool> {
    if !ACTIONS.contains(&action) { bail!("未知后台搜索操作"); }
    let text = name(profile, action);
    let handle = match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(text.as_ptr())) } {
        Ok(handle) => Kernel(handle), Err(error) if error.code().0 as u32 == 0x80070002 => return Ok(false), Err(error) => return Err(error.into()),
    };
    unsafe { SetEvent(handle.0)?; } Ok(true)
}

pub fn running(profile: &Path) -> bool {
    let text = name(profile, "instance");
    unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(text.as_ptr())) }.ok().map(Kernel).is_some()
}

pub struct Instance { _mutex: Kernel, events: Vec<Arc<Kernel>>, stop_requested: Arc<AtomicBool> }
impl Instance {
    pub fn acquire(profile: &Path) -> Result<Option<Self>> {
        // Create endpoints before publishing the instance mutex, so an opener
        // cannot observe a running instance with no command endpoint yet.
        let mut events = Vec::new();
        for action in ACTIONS { let text = name(profile, action);
            events.push(Arc::new(Kernel(unsafe { CreateEventW(None, false, false, PCWSTR(text.as_ptr()))? }))); }
        let text = name(profile, "instance");
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(text.as_ptr()))? };
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let mutex = Kernel(handle);
        if exists { return Ok(None); }
        Ok(Some(Self { _mutex: mutex, events, stop_requested: Arc::new(AtomicBool::new(false)) }))
    }
    pub fn stopping(&self) -> bool { self.stop_requested.load(Ordering::Acquire) }
    pub fn listen_native(&self, window:windows::Win32::Foundation::HWND, message:u32) -> Result<Listener> {
        let window=window.0 as isize;
        let stop = Arc::new(Kernel(unsafe { CreateEventW(None, true, false, None)? }));
        let events = self.events.clone(); let worker_stop = stop.clone();
        let process_stop = self.stop_requested.clone();
        let alive = Arc::new(AtomicBool::new(true)); let worker_alive = alive.clone();
        let worker = thread::Builder::new().name("zsdesk-search-commands".into()).spawn(move || {
            let mut handles: Vec<_> = events.iter().map(|event| event.0).collect(); handles.push(worker_stop.0);
            loop {
                let index = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) }.0 as usize;
                if index >= ACTIONS.len() || !worker_alive.load(Ordering::Acquire) { break; }
                if ACTIONS[index] == "stop" { process_stop.store(true, Ordering::Release); }
                let _=unsafe{windows::Win32::UI::WindowsAndMessaging::PostMessageW(Some(windows::Win32::Foundation::HWND(window as*mut _)),message,windows::Win32::Foundation::WPARAM(index),windows::Win32::Foundation::LPARAM(0))};
            }
        })?;
        Ok(Listener { stop, alive, worker: Some(worker) })
    }
}
pub struct Listener { stop: Arc<Kernel>, alive: Arc<AtomicBool>, worker: Option<JoinHandle<()>> }
impl Drop for Listener {
    fn drop(&mut self) { self.alive.store(false, Ordering::Release); let _ = unsafe { SetEvent(self.stop.0) };
        if let Some(worker) = self.worker.take() { let _ = worker.join(); } }
}

pub fn write_status(profile: &Path, error: Option<&str>, visible: bool, hotkey: &str, ready: bool) {
    let value = serde_json::json!({"schema":1,"pid":std::process::id(),"version":env!("CARGO_PKG_VERSION"),
        "executable":std::env::current_exe().ok(),"visible":visible,"hotkey":hotkey,"hotkey_registered":ready,"error":error});
    if let Ok(bytes) = serde_json::to_vec_pretty(&value) { let _ = atomic_write(&profile.with_file_name("quick-search-status.json"), &bytes); }
}

pub fn status(profile: &Path) -> serde_json::Value {
    let saved = fs::read(profile.with_file_name("quick-search-status.json")).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let mut win_e = fs::read(profile.with_file_name("win-e-status.json")).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let live = running(profile);
    if let Some(state) = win_e.as_mut() {
        let same_process = state["pid"].as_u64().zip(saved.as_ref().and_then(|status|status["pid"].as_u64())).is_some_and(|(a,b)|a==b);
        if !live || !same_process { state["active"] = false.into(); state["installed"] = false.into(); }
    }
    serde_json::json!({"running":live,"configuration":load(profile).ok(),"last_status":saved,"win_e":win_e})
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn global_shortcuts_require_deliberate_modifiers() {
        assert!(validate_hotkey("a").is_err()); assert!(validate_hotkey("Ctrl+Alt+Space").is_ok());
        assert!(validate_hotkey("F12").is_err()); assert!(validate_hotkey("F11").is_ok()); assert!(validate_hotkey("Ctrl+UnknownKey").is_err());
    }
    #[test] fn startup_arguments_preserve_spaces_and_trailing_separators() {
        assert_eq!(quote_argument("a b"), "\"a b\"");
        assert_eq!(quote_argument("C:\\"), "\"C:\\\\\"");
        assert_eq!(quote_argument("a\"b"), "\"a\\\"b\"");
    }
}
