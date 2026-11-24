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
    if fs::metadata(&path)?.len() > 65536 { bail!("{}", zsdesk_core::tr_format!("后台搜索配置过大")); }
    serde_json::from_slice(&fs::read(path)?).context(zsdesk_core::i18n::text("后台搜索配置格式无效"))
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    Ok(crate::storage::write_atomic(path,bytes)?)
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
    if bytes > 65536 { bail!("{}", zsdesk_core::tr_format!("登录启动项过大")); }
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

// Physical OEM and numpad keys have distinct Win32 virtual-key codes.
const EXTRA_KEYS: &[(&str,u32)] = &[
    ("Semicolon",0xba),("Plus",0xbb),("Comma",0xbc),("Minus",0xbd),("Period",0xbe),
    ("Slash",0xbf),("Grave",0xc0),("LeftBracket",0xdb),("Backslash",0xdc),("RightBracket",0xdd),("Quote",0xde),
    ("Insert",0x2d),("Num0",0x60),("Num1",0x61),("Num2",0x62),("Num3",0x63),("Num4",0x64),
    ("Num5",0x65),("Num6",0x66),("Num7",0x67),("Num8",0x68),("Num9",0x69),("NumMultiply",0x6a),
    ("NumAdd",0x6b),("NumSubtract",0x6d),("NumDecimal",0x6e),("NumDivide",0x6f)
];
pub(super) fn extra_key(vk:u32)->Option<&'static str>{EXTRA_KEYS.iter().find(|(_,code)|*code==vk).map(|(s,_)|*s)}
fn extra_chord(value:&str)->Option<(String,&'static str,u32)>{
    let (mods,last)=value.trim().rsplit_once('+')?;
    let name=match last.trim(){"/"=>"Slash","."=>"Period",","=>"Comma","-"=>"Minus","="=>"Plus",";"=>"Semicolon","["=>"LeftBracket","]"=>"RightBracket","\\"=>"Backslash","'"=>"Quote","`"=>"Grave",v=>v};
    EXTRA_KEYS.iter().find(|(key,_)|key.eq_ignore_ascii_case(name)).map(|(key,vk)|(format!("{mods}+A"),*key,*vk))
}

pub fn validate_hotkey(value: &str) -> Result<String> {
    if value.len() > 128 { bail!("{}", zsdesk_core::tr_format!("快捷键过长")); }
    let extra=extra_chord(value);
    let chord: ZsAccelerator = extra.as_ref().map_or(value.trim(),|(base,_,_)|base.as_str()).parse().map_err(|error| anyhow::anyhow!("{error}"))?;
    if chord.key() == ZsAcceleratorKey::Function(12) { bail!("{}", zsdesk_core::tr_format!("F12 为系统调试器保留，请选择其它快捷键")); }
    if !chord.uses_primary() && !chord.uses_alt() && !chord.uses_super() && !matches!(chord.key(), ZsAcceleratorKey::Function(_)) {
        bail!("{}", zsdesk_core::tr_format!("全局快捷键需包含 Ctrl、Alt、Win，或使用功能键"));
    }
    let normalized=chord.to_string().replace("Primary","Ctrl").replace("Super","Win");
    if let Some((_,key,_))=extra{let mods=normalized.strip_suffix("+A").context("Invalid shortcut")?;return Ok(format!("{mods}+{key}"));}
    Ok(normalized)
}

pub(super) fn native_hotkey(value:&str)->Result<(windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS,u32)>{
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let normalized=validate_hotkey(value)?;
    let extra=extra_chord(&normalized);
    let chord:ZsAccelerator=extra.as_ref().map_or(normalized.as_str(),|(base,_,_)|base.as_str()).parse().map_err(|error|anyhow::anyhow!("{error}"))?;
    let mut mods=MOD_NOREPEAT;
    if chord.uses_primary(){mods|=MOD_CONTROL;}if chord.uses_alt(){mods|=MOD_ALT;}
    if chord.uses_shift(){mods|=MOD_SHIFT;}if chord.uses_super(){mods|=MOD_WIN;}
    let key=match chord.key(){
        ZsAcceleratorKey::Character(value)=>value.to_ascii_uppercase()as u32,
        ZsAcceleratorKey::Function(n)=>VK_F1.0 as u32+u32::from(n)-1,
        ZsAcceleratorKey::Enter=>VK_RETURN.0 as u32,ZsAcceleratorKey::Escape=>VK_ESCAPE.0 as u32,
        ZsAcceleratorKey::Tab=>VK_TAB.0 as u32,ZsAcceleratorKey::Space=>VK_SPACE.0 as u32,
        ZsAcceleratorKey::Backspace=>VK_BACK.0 as u32,ZsAcceleratorKey::Delete=>VK_DELETE.0 as u32,
        ZsAcceleratorKey::Up=>VK_UP.0 as u32,ZsAcceleratorKey::Down=>VK_DOWN.0 as u32,
        ZsAcceleratorKey::Left=>VK_LEFT.0 as u32,ZsAcceleratorKey::Right=>VK_RIGHT.0 as u32,
        ZsAcceleratorKey::Home=>VK_HOME.0 as u32,ZsAcceleratorKey::End=>VK_END.0 as u32,
        ZsAcceleratorKey::PageUp=>VK_PRIOR.0 as u32,ZsAcceleratorKey::PageDown=>VK_NEXT.0 as u32,
    };Ok((mods,extra.map_or(key,|(_,_,vk)|vk)))
}

static PREF_WRITE:std::sync::Mutex<()>=std::sync::Mutex::new(());
pub fn apply(profile: &Path, preferences: QuickSearchPreferences) -> Result<Configuration> {
    let _lock=PREF_WRITE.lock().unwrap_or_else(|e|e.into_inner());
    apply_locked(profile,preferences)
}
pub(super) fn save_shortcut(profile:&Path,hotkey:String,enabled:bool)->Result<Configuration>{
    let _lock=PREF_WRITE.lock().unwrap_or_else(|e|e.into_inner());
    let mut config=load(profile)?;let previous_enabled=config.preferences.enabled;
    config.preferences.hotkey=validate_hotkey(&hotkey)?;
    config.preferences.enabled=enabled;
    if enabled==previous_enabled {
        atomic_write(&config_path(profile),&serde_json::to_vec_pretty(&config)?)?;Ok(config)
    }else{apply_locked(profile,config.preferences)}
}
pub(super) fn restore_rejected_chord(profile:&Path,rejected:&str,active:&str)->Result<()>{
    let _lock=PREF_WRITE.lock().unwrap_or_else(|e|e.into_inner());
    let mut config=load(profile)?;
    if config.preferences.enabled && config.preferences.hotkey==rejected{
        config.preferences.hotkey=active.into();
        atomic_write(&config_path(profile),&serde_json::to_vec_pretty(&config)?)?;
    }
    Ok(())
}
fn apply_locked(profile: &Path, mut preferences: QuickSearchPreferences) -> Result<Configuration> {
    preferences.hotkey = validate_hotkey(&preferences.hotkey)?;
    if !preferences.enabled { preferences.start_with_windows = false; }
    let previous = load(profile)?; let before = read_startup(profile)?;
    let executable = std::env::current_exe()?;
    let desired = preferences.start_with_windows.then(|| startup_command(profile, &executable));
    if before.is_some() && before != previous.startup_command && before != desired {
        bail!("{}", zsdesk_core::tr_format!("同名登录启动项已由其它设置修改，现有值已保留"));
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
        let directory = profile.parent().context(zsdesk_core::i18n::text("缺少主窗口配置目录"))?;
        anyhow::ensure!(executable.is_absolute() && directory.is_absolute(), "{}", zsdesk_core::tr_format!("登录启动需要绝对程序路径和配置目录"));
        Ok(format!("{} --profile-dir {}", quote_argument(&executable.to_string_lossy()),
            quote_argument(&directory.to_string_lossy())))
    }

    pub fn enabled(profile: &Path) -> Result<bool> {
        enabled_for(profile, &std::env::current_exe()?)
    }

    pub(crate) fn enabled_for(profile: &Path, executable: &Path) -> Result<bool> {
        let expected = command(profile, executable)?;
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
    if !ACTIONS.contains(&action) { bail!("{}", zsdesk_core::tr_format!("未知后台搜索操作")); }
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
    #[test]
    fn custom_shortcut_persists_and_a_stale_registration_error_cannot_revert_a_new_choice()->Result<()> {
        let temp=std::env::temp_dir().join(format!("zsdesk-custom-hotkey-{}",std::process::id()));
        fs::create_dir(&temp)?;
        struct Fixture(PathBuf);impl Drop for Fixture{fn drop(&mut self){let _=fs::remove_file(self.0.join("quick-search.json"));let _=fs::remove_dir(&self.0);}}
        let _fixture=Fixture(temp.clone());let profile=temp.join("explorer-session.json");
        let initial=Configuration{preferences:QuickSearchPreferences{enabled:true,hotkey:"Alt+F11".into(),start_with_windows:true},startup_command:Some("retained startup".into())};
        fs::write(config_path(&profile),serde_json::to_vec(&initial)?)?;
        save_shortcut(&profile,"Ctrl+Shift+K".into(),true)?;
        restore_rejected_chord(&profile,"Ctrl+Shift+X","Alt+F11")?;
        let prefs=load(&profile)?;assert_eq!(prefs.preferences.hotkey,"Ctrl+Shift+K");assert!(prefs.preferences.start_with_windows);
        assert_eq!(prefs.startup_command,initial.startup_command);
        restore_rejected_chord(&profile,"Ctrl+Shift+K","Alt+F11")?;
        assert_eq!(load(&profile)?.preferences.hotkey,"Alt+F11");
        assert!(save_shortcut(&profile,"F12".into(),true).is_err());assert_eq!(load(&profile)?.preferences.hotkey,"Alt+F11");
        Ok(())
    }
    #[test] fn global_shortcuts_require_deliberate_modifiers() {
        assert!(validate_hotkey("a").is_err()); assert!(validate_hotkey("Ctrl+Alt+Space").is_ok());
        assert!(validate_hotkey("F12").is_err()); assert!(validate_hotkey("F11").is_ok()); assert!(validate_hotkey("Ctrl+UnknownKey").is_err());
    }
    #[test]fn every_saved_hotkey_uses_the_same_native_key_parser(){
        for key in ["Enter","Escape","Tab","Space","Backspace","Delete","Up","Down","Left","Right","Home","End","PageUp","PageDown","A","1","F1","F24"]{
            let text=format!("Ctrl+Alt+{key}");assert!(validate_hotkey(&text).is_ok());assert!(native_hotkey(&text).is_ok(),"{text}");
        }
        for invalid in ["a","Ctrl+F12","Ctrl+UnknownKey","Ctrl+A+B"]{assert!(native_hotkey(invalid).is_err(),"{invalid}");}
    }
    #[test] fn startup_arguments_preserve_spaces_and_trailing_separators() {
        assert_eq!(quote_argument("a b"), "\"a b\"");
        assert_eq!(quote_argument("C:\\"), "\"C:\\\\\"");
        assert_eq!(quote_argument("a\"b"), "\"a\\\"b\"");
    }
}
