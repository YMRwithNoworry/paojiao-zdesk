//! Per-user file manager integration (规划 §6.1.8 系统打开接管与恢复).
//!
//! Only `HKCU\Software\Classes` is touched; `explorer.exe` is never patched or
//! injected. Before any write every affected value is recorded (data, type or
//! absence) in a backup file; a failed write rolls back immediately and
//! "恢复系统资源管理器" restores values still matching our recorded writes.
//! `Folder` (Control Panel and other
//! virtual folders) is left with Explorer; This PC has an explicit owned verb.

use std::{io, path::Path};

use serde::{Deserialize, Serialize};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{RegCloseKey, RegDeleteKeyValueW, RegOpenKeyExW, RegQueryValueExW, RegSetKeyValueW,
            HKEY, HKEY_CURRENT_USER, KEY_READ, REG_SZ, REG_VALUE_TYPE},
    },
};

const CLASSES: &str = r"Software\Classes";
const VERB: &str = "ZSDesk";
/// Classes receiving the "使用 ZSDesk 打开" verb.
const THIS_PC_CLASS:&str=r"CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
const THIS_PC_ROOT:&str=r"Software\Classes\CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
const VERB_CLASSES: [&str; 4] = ["Directory", "Drive", r"Directory\Background",THIS_PC_CLASS];
/// Classes whose default verb becomes ZSDesk in default mode.
const DEFAULT_CLASSES: [&str; 3] = ["Directory", "Drive",THIS_PC_CLASS];
const WIN_E_ROOT: &str = r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}";
const WIN_E_COMMAND: &str = r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command";
const BUILTIN_CLASSES:[&str;2]=["Directory","Drive"];
const BUILTIN_VERBS:[&str;2]=["open","explore"];
const VERB_OVERRIDES:[&str;6]=["LegacyDisable","NeverDefault","ProgrammaticAccessOnly","Extended","HideBasedOnVelocityId","ExplorerCommandHandler"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Integration {
    /// ZSDesk does not change folder opening.
    Off,
    /// Adds "使用 ZSDesk 打开" next to Explorer's own verbs.
    ContextMenu,
    /// Folders and drives open in ZSDesk by default.
    Default,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SavedValue { key: String, name: Option<String>, kind: Option<u32>, data: Option<Vec<u8>> }

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Backup {
    version: u32, values: Vec<SavedValue>, created_keys: Vec<String>,
    /// Exact values written (including deliberate absence), for conditional restoration.
    #[serde(default)] applied:Vec<SavedValue>,
}

fn wide(text: &str) -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() }
fn name_ptr(name: &Option<Vec<u16>>) -> PCWSTR { name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr())) }

fn key_exists(key: &str) -> bool {
    let path = wide(key);
    let mut handle = HKEY::default();
    let ok = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), None, KEY_READ, &mut handle) } == ERROR_SUCCESS;
    if ok { let _ = unsafe { RegCloseKey(handle) }; }
    ok
}

fn read_checked(key: &str, name: Option<&str>) -> io::Result<SavedValue> {
    let path = wide(key);
    let value_name = name.map(wide);
    let mut saved = SavedValue { key: key.into(), name: name.map(Into::into), kind: None, data: None };
    let mut handle = HKEY::default();
    let opened=unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), None, KEY_READ, &mut handle) };
    if opened.0==2{return Ok(saved);}
    if opened!=ERROR_SUCCESS{return Err(io::Error::from_raw_os_error(opened.0 as i32));}
    let result=(||{
        let mut kind = REG_VALUE_TYPE(0);
        let mut size = 0u32;
        let queried=unsafe { RegQueryValueExW(handle, name_ptr(&value_name), None, Some(&mut kind), None, Some(&mut size)) };
        if queried.0==2{return Ok(saved);}
        if queried!=ERROR_SUCCESS{return Err(io::Error::from_raw_os_error(queried.0 as i32));}
        if size>65536{return Err(io::Error::new(io::ErrorKind::InvalidData,"系统集成值过大"));}
        let mut data = vec![0u8; size as usize];
        let queried=unsafe { RegQueryValueExW(handle, name_ptr(&value_name), None, Some(&mut kind), Some(data.as_mut_ptr()), Some(&mut size)) };
        if queried!=ERROR_SUCCESS{return Err(io::Error::from_raw_os_error(queried.0 as i32));}
        data.truncate(size as usize);saved.kind=Some(kind.0);saved.data=Some(data);
        Ok(saved)
    })();
    let _ = unsafe { RegCloseKey(handle) };
    result
}

fn read_string(key: &str, name: Option<&str>) -> Option<String> {
    let value = read_checked(key, name).ok()?;
    if value.kind!=Some(REG_SZ.0){return None;}
    let data = value.data?;
    let units: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
    Some(String::from_utf16_lossy(&units))
}

fn write_raw(key: &str, name: Option<&str>, kind: u32, data: &[u8]) -> io::Result<()> {
    let path = wide(key);
    let value_name = name.map(wide);
    let status = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), name_ptr(&value_name), kind, Some(data.as_ptr().cast()), data.len() as u32) };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(io::Error::from_raw_os_error(status.0 as i32)) }
}

#[cfg(test)]
fn write_string(key: &str, name: Option<&str>, value: &str) -> io::Result<()> {
    let bytes: Vec<u8> = wide(value).iter().flat_map(|u| u.to_le_bytes()).collect();
    write_raw(key, name, REG_SZ.0, &bytes)
}

#[cfg(test)]
fn delete_tree(key: &str) {
    let path = wide(key);
    let _ = unsafe { windows::Win32::System::Registry::RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
    // RegDeleteTreeW empties the key; remove the key itself as well.
    let _ = unsafe { windows::Win32::System::Registry::RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
}

fn notify() {
    #[cfg(not(test))] {
        use windows::Win32::UI::Shell::{SHChangeNotify,SHCNE_ASSOCCHANGED,SHCNF_IDLIST};
        unsafe {SHChangeNotify(SHCNE_ASSOCCHANGED,SHCNF_IDLIST,None,None)};
    }
}

fn win_e_command(exe: &Path) -> String { format!("\"{}\" --win-e", exe.display()) }

fn win_e_keys() -> Vec<String> {
    vec![WIN_E_ROOT.into(), format!(r"{WIN_E_ROOT}\shell"),
        format!(r"{WIN_E_ROOT}\shell\opennewwindow"), WIN_E_COMMAND.into()]
}

fn win_e_owned(exe: &Path) -> bool {
    read_string(WIN_E_COMMAND,None).is_some_and(|value| value.eq_ignore_ascii_case(&win_e_command(exe)))
        && read_string(WIN_E_COMMAND,Some("DelegateExecute")).as_deref()==Some("")
}
pub fn win_e_enabled(exe:&Path)->bool {win_e_owned(exe)}
pub fn this_pc_enabled(exe:&Path)->bool{
    read_string(&format!(r"{THIS_PC_ROOT}\shell"),None).as_deref()==Some(VERB)
        &&read_string(&format!(r"{THIS_PC_ROOT}\shell\{VERB}\command"),None).is_some_and(|value|command_points_to(&value,exe))
}

/// Per-user folder takeover registered by this exact executable.
/// Win+E is a separate capability and is deliberately not part of this gate.
pub fn owns_folder_integration(exe:&Path)->bool{
    let expected=format!("\"{}\" --shell-open \"%V\"",exe.display());
    BUILTIN_CLASSES.into_iter().all(|class|{
        let shell=format!(r"{CLASSES}\{class}\shell");
        let owned_verb=format!(r"{shell}\{VERB}");
        read_string(&shell,None).as_deref()==Some(VERB)
            &&read_string(&format!(r"{shell}\{VERB}\command"),None).is_some_and(|command|command.eq_ignore_ascii_case(&expected))
            &&read_string(&format!(r"{owned_verb}\command"),Some("DelegateExecute")).as_deref()==Some("")
            &&VERB_OVERRIDES.into_iter().all(|name|read_checked(&owned_verb,Some(name)).is_ok_and(|value|value.kind.is_none()))
            &&BUILTIN_VERBS.into_iter().all(|verb|{
                let root=format!(r"{shell}\{verb}");let command=format!(r"{root}\command");
                read_string(&command,None).is_some_and(|value|value.eq_ignore_ascii_case(&expected))
                    &&read_string(&command,Some("DelegateExecute")).as_deref()==Some("")
                    &&VERB_OVERRIDES.into_iter().all(|name|read_checked(&root,Some(name)).is_ok_and(|value|value.kind.is_none()))
            })
    })
}

/// A still-selected ZSDesk default whose canonical handlers were later changed.
/// Report the conflict instead of repeatedly overwriting another program.
pub fn folder_integration_needs_attention(exe:&Path)->bool{
    current()==Integration::Default
        &&registered_command().is_some_and(|command|command_points_to(&command,exe))
        &&!owns_folder_integration(exe)
}

fn remove_created_key(key: &str) {
    // All class keys are shared. Remove only empty keys we created,
    // keeping values subsequently added by another application or the user.
    let path=wide(key);let mut handle=HKEY::default();
    if unsafe {RegOpenKeyExW(HKEY_CURRENT_USER,PCWSTR(path.as_ptr()),None,KEY_READ,&mut handle)}!=ERROR_SUCCESS{return;}
    let mut subkeys=0;let mut values=0;
    let status=unsafe {windows::Win32::System::Registry::RegQueryInfoKeyW(handle,None,None,None,Some(&mut subkeys),None,None,Some(&mut values),None,None,None,None)};
    let _=unsafe {RegCloseKey(handle)};
    if status==ERROR_SUCCESS && subkeys==0 && values==0 {let _=unsafe {windows::Win32::System::Registry::RegDeleteKeyW(HKEY_CURRENT_USER,PCWSTR(path.as_ptr()))};}
}

fn save_backup(path:&Path,backup:&Backup)->io::Result<()> {
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MoveFileExW,MOVEFILE_REPLACE_EXISTING,MOVEFILE_WRITE_THROUGH};
    if let Some(parent)=path.parent(){std::fs::create_dir_all(parent)?;}
    let temporary=path.with_extension(format!("{}.pending",std::process::id()));
    let mut file=std::fs::File::create(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(backup).map_err(io::Error::other)?)?;file.sync_all()?;drop(file);
    let from:Vec<u16>=temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let to:Vec<u16>=path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {MoveFileExW(PCWSTR(from.as_ptr()),PCWSTR(to.as_ptr()),MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH)}.map_err(|error|io::Error::other(error.to_string()))
}

fn validate_backup(backup:&Backup,exe:&Path)->io::Result<()> {
    let allowed=planned_values(Integration::Default,exe);let (_,keys)=plan(Integration::Default,exe);
    let mut seen=std::collections::HashSet::new();
    let valid_value=|value:&SavedValue|value.kind.is_some()==value.data.is_some()
        &&value.data.as_ref().is_none_or(|bytes|bytes.len()<=65536)
        &&allowed.iter().any(|item|same_value_id(item,value));
    if ![1,2].contains(&backup.version) || backup.values.len()>allowed.len() || backup.created_keys.len()>keys.len()
        || backup.values.iter().any(|value| value.kind.is_some()!=value.data.is_some()
            || !valid_value(value)
            || !seen.insert((&value.key,&value.name))
            )
        || backup.created_keys.iter().any(|key|!keys.contains(key))
        || backup.created_keys.iter().collect::<std::collections::HashSet<_>>().len()!=backup.created_keys.len()
        || (backup.version==1&&!backup.applied.is_empty())
        || (backup.version==2&&(backup.applied.len()!=backup.values.len()
            ||backup.applied.iter().any(|value|!valid_value(value)||backup.values.iter().filter(|old|same_value_id(old,value)).count()!=1)
            ||backup.applied.iter().enumerate().any(|(index,value)|backup.applied[..index].iter().any(|old|same_value_id(old,value))))) {
        return Err(io::Error::new(io::ErrorKind::InvalidData,"接管备份格式无效，未修改系统集成"));
    }
    Ok(())
}

/// Completes an existing owned default-manager registration without replaying
/// or replacing the user's original folder/drive restoration records.
pub fn upgrade_default(exe:&Path,backup_path:&Path)->io::Result<bool> {
    if current()!=Integration::Default || !registered_command().is_some_and(|command|command_points_to(&command,exe)) {return Ok(false);}
    let bytes=std::fs::read(backup_path)?;
    let mut backup:Backup=serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    validate_backup(&backup,exe)?;
    let migrated=migrate_backup(&mut backup,exe);
    let protected=changed_groups(&backup)?;
    let (_,keys)=plan(Integration::Default,exe);
    let planned:Vec<_>=planned_values(Integration::Default,exe).into_iter()
        .filter(|value|!backup.values.iter().any(|saved|same_value_id(saved,value))&&!protected.contains(&value_group(value))
            &&BUILTIN_CLASSES.into_iter().all(|class|{
                let shell=format!(r"{CLASSES}\{class}\shell");
                let canonical=BUILTIN_VERBS.into_iter().any(|verb|value.key==format!(r"{shell}\{verb}")||value.key==format!(r"{shell}\{verb}\command"));
                !canonical||(read_string(&shell,None).as_deref()==Some(VERB)
                    &&read_string(&format!(r"{shell}\{VERB}\command"),None).is_some_and(|command|command_points_to(&command,exe)))
            })).collect();
    if planned.is_empty(){if migrated{save_backup(backup_path,&backup)?;}return Ok(migrated);}
    let before:Vec<_>=planned.iter().map(|value|read_checked(&value.key,value.name.as_deref())).collect::<io::Result<_>>()?;
    backup.values.extend(before.iter().cloned());
    backup.applied.extend(planned.iter().cloned());
    let created:Vec<_>=keys.into_iter().filter(|key|!key_exists(key)&&!backup.created_keys.contains(key)
        &&planned.iter().any(|value|value.key==*key||value.key.starts_with(&format!(r"{key}\")))).collect();
    backup.created_keys.extend(created.iter().cloned());
    save_backup(backup_path,&backup)?;
    let result=write_plan(&planned,&before);
    if let Err(error)=result {
        rollback_values(&planned,&before)?;
        for key in created.iter().rev(){remove_created_key(key);}
        // Keep the complete recovery record on every failure.
        notify();return Err(error);
    }
    notify();Ok(true)
}

/// Values to write for `level` (key, value name, data).
fn plan(level: Integration, exe: &Path) -> (Vec<(String, Option<&'static str>, String)>, Vec<String>) {
    let exe = exe.display().to_string();
    let mut values = Vec::new();
    let mut verb_keys = Vec::new();
    if level == Integration::Off { return (values, verb_keys); }
    verb_keys.extend([THIS_PC_ROOT.to_string(),format!(r"{THIS_PC_ROOT}\shell")]);
    for class in VERB_CLASSES {
        let verb = format!(r"{CLASSES}\{class}\shell\{VERB}");
        values.push((verb.clone(), None, "使用 ZSDesk 打开".to_string()));
        values.push((verb.clone(), Some("Icon"), format!("\"{exe}\",0")));
        values.push((format!(r"{verb}\command"), None, if class==THIS_PC_CLASS{format!("\"{exe}\" --this-pc")}else{format!("\"{exe}\" --shell-open \"%V\"" )}));
        values.push((format!(r"{verb}\command"),Some("DelegateExecute"),String::new()));
        verb_keys.push(verb.clone());verb_keys.push(format!(r"{verb}\command"));
    }
    if level == Integration::Default {
        for class in DEFAULT_CLASSES { values.push((format!(r"{CLASSES}\{class}\shell"), None, VERB.to_string())); }
        values.push((WIN_E_COMMAND.into(),None,win_e_command(Path::new(&exe))));
        values.push((WIN_E_COMMAND.into(),Some("DelegateExecute"),String::new()));
        verb_keys.extend(win_e_keys());
        for class in BUILTIN_CLASSES {for builtin in BUILTIN_VERBS {
            let verb=format!(r"{CLASSES}\{class}\shell\{builtin}");let command=format!(r"{verb}\command");
            values.push((command.clone(),None,format!("\"{exe}\" --shell-open \"%V\"")));
            values.push((command.clone(),Some("DelegateExecute"),String::new()));
            verb_keys.extend([verb,command]);
        }}
    }
    (values, verb_keys)
}

fn same_value_id(left:&SavedValue,right:&SavedValue)->bool{left.key==right.key&&left.name==right.name}
fn string_value(key:String,name:Option<&str>,text:&str)->SavedValue{
    SavedValue{key,name:name.map(String::from),kind:Some(REG_SZ.0),data:Some(wide(text).iter().flat_map(|unit|unit.to_le_bytes()).collect())}
}
fn planned_values(level:Integration,exe:&Path)->Vec<SavedValue>{
    let mut values:Vec<_>=plan(level,exe).0.into_iter().map(|(key,name,text)|string_value(key,name,&text)).collect();
    if level!=Integration::Off{for class in VERB_CLASSES{for name in VERB_OVERRIDES{
        values.push(SavedValue{key:format!(r"{CLASSES}\{class}\shell\{VERB}"),name:Some(name.into()),kind:None,data:None});
    }}}
    if level==Integration::Default{for class in BUILTIN_CLASSES{for verb in BUILTIN_VERBS{for name in VERB_OVERRIDES{
        values.push(SavedValue{key:format!(r"{CLASSES}\{class}\shell\{verb}"),name:Some(name.into()),kind:None,data:None});
    }}}}
    values
}
#[cfg(test)]
fn builtin_key(key:&str)->bool{BUILTIN_CLASSES.into_iter().any(|class|BUILTIN_VERBS.into_iter().any(|verb|{
    let root=format!(r"{CLASSES}\{class}\shell\{verb}");key==root||key.starts_with(&format!(r"{root}\"))
}))}
fn value_group(value:&SavedValue)->String{
    if value.key==WIN_E_COMMAND{return WIN_E_COMMAND.trim_end_matches(r"\command").into();}
    for class in DEFAULT_CLASSES{let shell=format!(r"{CLASSES}\{class}\shell");
        if value.key==shell{return format!(r"{shell}\{VERB}");}}
    value.key.strip_suffix(r"\command").unwrap_or(&value.key).into()
}
fn receipts(backup:&Backup,exe:&Path)->Vec<SavedValue>{
    if backup.version==2{return backup.applied.clone();}
    planned_values(Integration::Default,exe).into_iter().filter(|value|backup.values.iter().any(|saved|same_value_id(saved,value))).collect()
}
fn migrate_backup(backup:&mut Backup,exe:&Path)->bool{
    if backup.version!=1{return false;}
    backup.applied=receipts(backup,exe);backup.version=2;
    // Older records listed a newly created verb root but omitted its command
    // child. Add only descendants of those proven-new roots; cleanup still
    // removes empty keys exclusively.
    let roots=backup.created_keys.clone();
    for key in plan(Integration::Default,exe).1{
        if !backup.created_keys.contains(&key)&&roots.iter().any(|root|key.starts_with(&format!(r"{root}\"))){backup.created_keys.push(key);}
    }
    true
}
fn changed_groups(backup:&Backup)->io::Result<std::collections::HashSet<String>>{
    let mut groups=std::collections::HashSet::new();
    for expected in &backup.applied{let current=read_checked(&expected.key,expected.name.as_deref())?;
        if current!=*expected&&backup.values.iter().find(|saved|same_value_id(saved,expected))!=Some(&current){groups.insert(value_group(expected));}}
    Ok(groups)
}
fn write_plan(planned:&[SavedValue],before:&[SavedValue])->io::Result<()>{
    for (value,original) in planned.iter().zip(before){
        if read_checked(&value.key,value.name.as_deref())?!=*original{return Err(io::Error::other("系统集成项已被其他程序更改，未覆盖该项"));}
        restore_value(value)?;
    }
    for value in planned{if read_checked(&value.key,value.name.as_deref())?!=*value{return Err(io::Error::other("接管配置回读不一致，恢复记录已保留"));}}
    Ok(())
}
fn rollback_values(planned:&[SavedValue],before:&[SavedValue])->io::Result<()>{
    for (expected,original) in planned.iter().zip(before).rev(){
        if read_checked(&expected.key,expected.name.as_deref())?==*expected{restore_value(original)?;}
    }Ok(())
}

/// Current integration level registered for this user.
pub fn current() -> Integration {
    let default = read_string(&format!(r"{CLASSES}\Directory\shell"), None);
    if default.as_deref() == Some(VERB) { return Integration::Default; }
    if key_exists(&format!(r"{CLASSES}\Directory\shell\{VERB}\command")) { return Integration::ContextMenu; }
    Integration::Off
}

/// Executable referenced by the registered verb, if any (health check after moves/updates).
pub fn registered_command() -> Option<String> { read_string(&format!(r"{CLASSES}\Directory\shell\{VERB}\command"), None) }

fn command_points_to(command: &str, exe: &Path) -> bool {
    let Some((path, arguments)) = command.trim().strip_prefix('"').and_then(|text| text.split_once('"')) else { return false };
    (arguments.trim_start().starts_with("--shell-open ")||arguments.trim()=="--this-pc")
        && path.replace('/', "\\").eq_ignore_ascii_case(&exe.to_string_lossy().replace('/', "\\"))
}

fn restore_value(value: &SavedValue) -> io::Result<()> {
    if let (Some(kind), Some(data)) = (value.kind, &value.data) { return write_raw(&value.key, value.name.as_deref(), kind, data); }
    let path = wide(&value.key);
    let name = value.name.as_deref().map(wide);
    let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), name_ptr(&name)) };
    if status == ERROR_SUCCESS || status.0 == 2 { Ok(()) } else { Err(io::Error::from_raw_os_error(status.0 as i32)) }
}

/// Uninstaller cleanup is limited to verbs still owned by this executable.
/// Later choices made by the user or another installation remain in place.
/// The profile and its original restoration record are retained.
pub fn restore_owned(exe: &Path, backup_path: &Path) -> io::Result<bool> {
    let mut backup:Backup = match std::fs::read(backup_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let applied=planned_values(Integration::Default,exe);
            let values=applied.iter().map(|value|SavedValue{key:value.key.clone(),name:value.name.clone(),kind:None,data:None}).collect();
            Backup{version:2,values,created_keys:plan(Integration::Default,exe).1,applied}
        },
        Err(error) => return Err(error),
    };
    validate_backup(&backup,exe)?;
    migrate_backup(&mut backup,exe);
    let changed=replay_owned(&backup,Some(exe))?;if changed{notify();}Ok(changed)
}

/// Switches to `level`, keeping a backup of everything it changes in `backup_path`.
pub fn apply(level: Integration, exe: &Path, backup_path: &Path) -> io::Result<()> {
    if backup_path.try_exists()? {
        let mut backup:Backup=serde_json::from_slice(&std::fs::read(backup_path)?).map_err(io::Error::other)?;
        validate_backup(&backup,exe)?;
        migrate_backup(&mut backup,exe);
        if level!=Integration::Off&&!changed_groups(&backup)?.is_empty(){
            return Err(io::Error::other("系统集成已被其他程序或用户更改，已保留后续设置；请先恢复本应用的接管后重新选择"));
        }
        replay_owned(&backup,None)?;std::fs::remove_file(backup_path)?;
    }
    else {restore_owned(exe,backup_path)?;}
    if level == Integration::Off { notify();return Ok(()); }
    let values=planned_values(level,exe);let (_,verb_keys)=plan(level,exe);
    let mut backup = Backup { version: 2, applied:values.clone(), ..Default::default() };
    for value in &values { backup.values.push(read_checked(&value.key,value.name.as_deref())?); }
    backup.created_keys = verb_keys.into_iter().filter(|k| !key_exists(k)).collect();
    save_backup(backup_path,&backup)?;
    if let Err(error)=write_plan(&values,&backup.values){
        if rollback_values(&values,&backup.values).is_ok(){
            for key in backup.created_keys.iter().rev(){remove_created_key(key);}
            let _=std::fs::remove_file(backup_path);
        }
        notify();return Err(error);
    }
    notify();
    Ok(())
}

fn saved_string(value:&SavedValue)->Option<String>{
    if value.kind!=Some(REG_SZ.0){return None;}
    let bytes=value.data.as_ref()?;let units:Vec<_>=bytes.chunks_exact(2).map(|pair|u16::from_le_bytes([pair[0],pair[1]])).take_while(|unit|*unit!=0).collect();
    Some(String::from_utf16_lossy(&units))
}
fn replay_owned(backup:&Backup,exe:Option<&Path>)->io::Result<bool>{
    let mut owned=std::collections::HashSet::new();
    for expected in backup.applied.iter().filter(|value|value.name.is_none()&&value.key.ends_with(r"\command")){
        if exe.is_some_and(|exe|!saved_string(expected).is_some_and(|command|command_points_to(&command,exe)||command.eq_ignore_ascii_case(&win_e_command(exe)))){continue;}
        if read_checked(&expected.key,None)?!=*expected{continue;}
        if let Some(delegate)=backup.applied.iter().find(|value|value.key==expected.key&&value.name.as_deref()==Some("DelegateExecute")){
            if read_checked(&delegate.key,delegate.name.as_deref())?!=*delegate{continue;}
        }
        let group=value_group(expected);
        if let Some(handler)=backup.applied.iter().find(|value|value.key==group&&value.name.as_deref()==Some("ExplorerCommandHandler")){
            if read_checked(&handler.key,handler.name.as_deref())?!=*handler{continue;}
        }
        owned.insert(group);
    }
    let mut changed=false;
    for original in backup.values.iter().rev(){
        let Some(expected)=backup.applied.iter().find(|value|same_value_id(value,original))else{continue;};
        if owned.contains(&value_group(expected))&&read_checked(&expected.key,expected.name.as_deref())?==*expected{
            if expected!=original{restore_value(original)?;changed=true;}
        }
    }
    for key in backup.created_keys.iter().rev().filter(|key|owned.iter().any(|group|*key==group
        ||key.starts_with(&format!(r"{group}\"))||group.starts_with(&format!(r"{key}\")))) { remove_created_key(key); }
    Ok(changed)
}

/// Restores Explorer's folder opening from the backup ("恢复系统资源管理器").
pub fn restore(backup_path: &Path) -> io::Result<()> {
    match std::fs::read(backup_path) {
        Ok(bytes) => {
            let mut backup: Backup = serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("接管备份已损坏，未修改注册表：{e}")))?;
            let exe=std::env::current_exe()?;validate_backup(&backup,&exe)?;
            migrate_backup(&mut backup,&exe);
            replay_owned(&backup,None)?;
            std::fs::remove_file(backup_path)?;
            notify();
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Ok(exe)=std::env::current_exe(){restore_owned(&exe,backup_path)?;}
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    static REGISTRY_TEST:std::sync::Mutex<()>=std::sync::Mutex::new(());
    #[test]
    fn malformed_or_unrelated_restore_records_are_rejected() {
        let invalid=Backup{version:1,values:vec![SavedValue{key:r"Software\OtherApp".into(),name:None,kind:None,data:None}],..Default::default()};
        assert!(validate_backup(&invalid,Path::new("z.exe")).is_err());
        let (values,keys)=plan(Integration::ContextMenu,Path::new("z.exe"));
        assert!(values.iter().all(|(key,_,_)|!key.starts_with(WIN_E_ROOT)));
        assert!(keys.iter().all(|key|!key.starts_with(WIN_E_ROOT)));
    }

    #[test]
    #[ignore="Uses a process-local registry override and ZSDESK_NATIVE_TEST_OUTPUT; never dispatches the user's Win+E"]
    fn win_e_registration_upgrade_and_owned_restore()->Result<(),Box<dyn std::error::Error>> {
        let _serial=REGISTRY_TEST.lock().unwrap_or_else(|error|error.into_inner());
        use windows::Win32::System::Registry::{RegCreateKeyExW,RegOverridePredefKey,KEY_ALL_ACCESS,REG_OPTION_NON_VOLATILE,REG_EXPAND_SZ};
        use std::path::PathBuf;
        struct Sandbox{key:String,handle:HKEY,active:bool}
        impl Drop for Sandbox {fn drop(&mut self){
            if self.active {let _=unsafe{RegOverridePredefKey(HKEY_CURRENT_USER,None)};self.active=false;}
            let _=unsafe{RegCloseKey(self.handle)};delete_tree(&self.key);
        }}
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").ok_or("missing test output")?);
        assert!(output.is_absolute());std::fs::create_dir_all(&output)?;
        let key=format!(r"Software\ZSDesk.WinE.Regression.{}",std::process::id());
        assert!(!key_exists(&key),"test registry root already exists");
        let raw=wide(&key);let mut handle=HKEY::default();
        unsafe{RegCreateKeyExW(HKEY_CURRENT_USER,PCWSTR(raw.as_ptr()),None,PCWSTR::null(),REG_OPTION_NON_VOLATILE,KEY_ALL_ACCESS,None,&mut handle,None)}.ok()?;
        let mut sandbox=Sandbox{key,handle,active:false};
        unsafe{RegOverridePredefKey(HKEY_CURRENT_USER,Some(handle))}.ok()?;sandbox.active=true;
        let exe=Path::new(r"C:\Program Files\测试 ZSDesk\ZSDesk.exe");
        let backup_file=output.join("win-e-test-backup.json");
        assert!(!backup_file.exists(),"previous test backup requires inspection");
        let result=(||->Result<(),Box<dyn std::error::Error>>{
            write_string(r"Software\Classes\Directory\shell",None,"previous-manager")?;
            let original:Vec<u8>=wide(r"%SystemRoot%\explorer.exe").iter().flat_map(|unit|unit.to_le_bytes()).collect();
            write_raw(WIN_E_COMMAND,None,REG_EXPAND_SZ.0,&original)?;
            write_string(WIN_E_COMMAND,Some("DelegateExecute"),"{11dbb47c-a525-400b-9e80-a54615a090c0}")?;
            let old_command=read_checked(WIN_E_COMMAND,None)?;let old_delegate=read_checked(WIN_E_COMMAND,Some("DelegateExecute"))?;
            apply(Integration::Default,exe,&backup_file)?;
            assert!(win_e_enabled(exe));assert!(this_pc_enabled(exe));assert_eq!(current(),Integration::Default);
            assert!(restore_owned(exe,&backup_file)?);
            assert_eq!(read_checked(WIN_E_COMMAND,None)?,old_command);
            assert_eq!(read_checked(WIN_E_COMMAND,Some("DelegateExecute"))?,old_delegate);
            assert_eq!(read_string(r"Software\Classes\Directory\shell",None).as_deref(),Some("previous-manager"));
            assert!(!key_exists(THIS_PC_ROOT),"empty This PC override must be removed on restore");
            restore(&backup_file)?;
            delete_tree(WIN_E_ROOT);
            let (mut legacy_values,mut legacy_keys)=plan(Integration::Default,exe);
            legacy_values.retain(|(key,_,_)|!key.starts_with(WIN_E_ROOT)&&!key.starts_with(THIS_PC_ROOT)&&!builtin_key(key));
            legacy_keys.retain(|key|!key.starts_with(WIN_E_ROOT)&&!key.starts_with(THIS_PC_ROOT)&&!builtin_key(key));
            let mut legacy=Backup{version:1,..Default::default()};
            for (key,name,_) in &legacy_values{legacy.values.push(read_checked(key,*name)?);}
            legacy.created_keys=legacy_keys.into_iter().filter(|key|!key_exists(key)).collect();
            save_backup(&backup_file,&legacy)?;
            for (key,name,value) in &legacy_values{write_string(key,*name,value)?;}
            assert!(upgrade_default(exe,&backup_file)?);assert!(win_e_enabled(exe));assert!(this_pc_enabled(exe));
            let upgraded:Backup=serde_json::from_slice(&std::fs::read(&backup_file)?)?;
            assert_eq!(&upgraded.values[..legacy.values.len()],&legacy.values);
            assert!(!upgrade_default(exe,&backup_file)?);
            restore(&backup_file)?;
            assert!(!key_exists(WIN_E_ROOT),"empty user CLSID must not shadow the system class");
            apply(Integration::Default,exe,&backup_file)?;
            write_string(WIN_E_COMMAND,None,r#""C:\Other\Manager.exe""#)?;
            assert!(restore_owned(exe,&backup_file)?);
            assert_eq!(read_string(WIN_E_COMMAND,None).as_deref(),Some(r#""C:\Other\Manager.exe""#));
            restore(&backup_file)?;
            Ok(())
        })();
        unsafe{RegOverridePredefKey(HKEY_CURRENT_USER,None)}.ok()?;sandbox.active=false;
        let registry_key=sandbox.key.clone();drop(sandbox);
        let cleaned=!key_exists(&registry_key);
        let report=serde_json::json!({"state":if result.is_ok()&&cleaned{"passed"}else{"failed"},"pid":std::process::id(),
            "registry_override_process_only":true,"registry_cleanup":cleaned,"original_value_type_restored":result.is_ok(),
            "legacy_backup_preserved":result.is_ok(),"foreign_command_preserved":result.is_ok(),"keyboard_input_sent":false,
            "error":result.as_ref().err().map(|error|error.to_string())});
        std::fs::write(output.join("win-e.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
        result?;assert!(cleaned);Ok(())
    }
    #[test]
    fn uninstaller_only_recognizes_the_registered_executable_and_verb() {
        let exe = Path::new(r"C:\Apps\ZSDesk\ZSDesk.exe");
        assert!(command_points_to(r#""c:\apps\zsdesk\zsdesk.exe" --shell-open "%V""#, exe));
        assert!(!command_points_to(r#""C:\Other\ZSDesk.exe" --shell-open "%V""#, exe));
        assert!(!command_points_to(r#""C:\Apps\ZSDesk\ZSDesk.exe" --desktop"#, exe));
        assert!(!command_points_to(r#"cmd /c "C:\Apps\ZSDesk\ZSDesk.exe" --shell-open "%V""#, exe));
    }
    #[test]
    fn plans_only_touch_per_user_classes() {
        let (values, keys) = plan(Integration::Default, Path::new(r"C:\Apps\ZSDesk.exe"));
        assert!(values.iter().all(|(k, _, _)| k.starts_with(r"Software\Classes\")));
        assert!(values.iter().any(|(k, n, v)| k.ends_with(r"Directory\shell") && n.is_none() && v == "ZSDesk"));
        assert!(!values.iter().any(|(k, _, _)| k.contains(r"\Folder\")), "virtual folders stay with Explorer");
        assert!(values.iter().any(|(_, _, v)| v == r#""C:\Apps\ZSDesk.exe" --shell-open "%V""#));
        assert_eq!(keys.iter().collect::<std::collections::HashSet<_>>().len(),keys.len());
        for class in BUILTIN_CLASSES{for verb in BUILTIN_VERBS{
            let command=format!(r"{CLASSES}\{class}\shell\{verb}\command");
            assert!(values.iter().any(|(key,name,value)|key==&command&&name.is_none()&&value==r#""C:\Apps\ZSDesk.exe" --shell-open "%V""#));
            assert!(values.iter().any(|(key,name,value)|key==&command&&*name==Some("DelegateExecute")&&value.is_empty()));
        }}
        let desired=planned_values(Integration::Default,Path::new("z.exe"));
        assert_eq!(desired.iter().filter(|value|builtin_key(&value.key)&&value.kind.is_none()).count(),BUILTIN_CLASSES.len()*BUILTIN_VERBS.len()*VERB_OVERRIDES.len());
        assert!(values.iter().any(|(key,name,value)|key==&format!(r"{THIS_PC_ROOT}\shell")&&name.is_none()&&value==VERB));
        assert!(values.iter().any(|(key,name,value)|key==&format!(r"{THIS_PC_ROOT}\shell\{VERB}\command")&&name.is_none()&&value==r#""C:\Apps\ZSDesk.exe" --this-pc"#));
        assert!(values.iter().any(|(key,name,value)|key==WIN_E_COMMAND && name.is_none() && value==r#""C:\Apps\ZSDesk.exe" --win-e"#));
        assert!(values.iter().any(|(key,name,value)|key==WIN_E_COMMAND && *name==Some("DelegateExecute") && value.is_empty()));
        let (context, _) = plan(Integration::ContextMenu, Path::new("z.exe"));
        assert!(!context.iter().any(|(key,_,_)|builtin_key(key)),"context-menu mode leaves canonical verbs unchanged");
        assert!(!context.iter().any(|(k, n, _)| k.ends_with(r"\shell") && n.is_none()), "context mode keeps Explorer as default");
        assert!(plan(Integration::Off, Path::new("z.exe")).0.is_empty());
    }

    #[test]
    fn backup_v2_requires_one_exact_receipt_per_original_value(){
        let exe=Path::new(r"C:\Apps\ZSDesk.exe");let applied=planned_values(Integration::Default,exe);
        let values=applied.iter().map(|value|SavedValue{key:value.key.clone(),name:value.name.clone(),kind:None,data:None}).collect();
        let backup=Backup{version:2,values,applied,..Default::default()};assert!(validate_backup(&backup,exe).is_ok());
        let mut missing=backup.clone();missing.applied.pop();assert!(validate_backup(&missing,exe).is_err());
        let mut duplicate=backup.clone();duplicate.applied[1]=duplicate.applied[0].clone();assert!(validate_backup(&duplicate,exe).is_err());
        let mut unrelated=backup.clone();unrelated.applied[0].key=r"Software\Classes\Folder\shell\open\command".into();
        assert!(validate_backup(&unrelated,exe).is_err());
        let mut old=backup;old.version=1;old.applied.clear();
        let decoded:Backup=serde_json::from_slice(&serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(receipts(&decoded,exe).len(),old.values.len());assert!(validate_backup(&decoded,exe).is_ok());
    }

    #[test]
    #[ignore="Uses a process-local registry override and isolated output; no Shell launch or keyboard input"]
    fn canonical_verbs_upgrade_restore_and_foreign_write_protection()->Result<(),Box<dyn std::error::Error>>{
        use windows::Win32::System::Registry::{RegCreateKeyExW,RegOverridePredefKey,KEY_ALL_ACCESS,REG_OPTION_NON_VOLATILE,REG_EXPAND_SZ,REG_DWORD};
        use std::path::PathBuf;
        let _serial=REGISTRY_TEST.lock().unwrap_or_else(|error|error.into_inner());
        struct Sandbox{key:String,handle:HKEY,active:bool}
        impl Drop for Sandbox{fn drop(&mut self){
            if self.active{let _=unsafe{RegOverridePredefKey(HKEY_CURRENT_USER,None)};}
            let _=unsafe{RegCloseKey(self.handle)};delete_tree(&self.key);
        }}
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").ok_or("missing isolated test output")?);
        assert!(output.is_absolute());std::fs::create_dir_all(&output)?;
        let key=format!(r"Software\ZSDesk.CanonicalVerbs.Regression.{}",std::process::id());assert!(!key_exists(&key));
        let raw=wide(&key);let mut handle=HKEY::default();
        unsafe{RegCreateKeyExW(HKEY_CURRENT_USER,PCWSTR(raw.as_ptr()),None,PCWSTR::null(),REG_OPTION_NON_VOLATILE,KEY_ALL_ACCESS,None,&mut handle,None)}.ok()?;
        let mut sandbox=Sandbox{key,handle,active:false};
        unsafe{RegOverridePredefKey(HKEY_CURRENT_USER,Some(handle))}.ok()?;sandbox.active=true;
        let exe=Path::new(r"C:\Program Files\ZSDesk 测试\ZSDesk.exe");let backup_path=output.join("canonical-verbs-test-backup.json");
        assert!(!backup_path.exists(),"existing recovery evidence must be inspected first");
        let result=(||->Result<(),Box<dyn std::error::Error>>{
            let folder=r"Software\Classes\Folder\shell\open\command";
            write_string(folder,None,"virtual-folder-handler")?;let folder_before=read_checked(folder,None)?;
            for class in BUILTIN_CLASSES{for verb in BUILTIN_VERBS{
                let root=format!(r"{CLASSES}\{class}\shell\{verb}");let command=format!(r"{root}\command");
                let bytes:Vec<_>=wide(r#""%ProgramFiles%\Previous Manager\manager.exe" "%1""#).iter().flat_map(|unit|unit.to_le_bytes()).collect();
                write_raw(&command,None,REG_EXPAND_SZ.0,&bytes)?;
                write_string(&command,Some("DelegateExecute"),"{11111111-2222-3333-4444-555555555555}")?;
                for (index,name) in VERB_OVERRIDES.iter().enumerate(){
                    if *name=="HideBasedOnVelocityId"{write_raw(&root,Some(name),REG_DWORD.0,&6527944u32.to_le_bytes())?;}
                    else{write_string(&root,Some(name),&format!("original-{index}"))?;}
                }
                write_string(&root,Some("UnrelatedValue"),"keep")?;
            }}
            let original:Vec<_>=planned_values(Integration::Default,exe).into_iter().filter(|value|builtin_key(&value.key))
                .map(|value|read_checked(&value.key,value.name.as_deref())).collect::<io::Result<_>>()?;
            let owned_root=r"Software\Classes\Directory\shell\ZSDesk";
            write_string(owned_root,Some("LegacyDisable"),"original-hidden-private-verb")?;
            let original_private_hidden=read_checked(owned_root,Some("LegacyDisable"))?;
            apply(Integration::Default,exe,&backup_path)?;
            assert!(owns_folder_integration(exe));
            assert!(!folder_integration_needs_attention(exe));
            assert!(read_checked(owned_root,Some("LegacyDisable"))?.kind.is_none());
            write_string(owned_root,Some("LegacyDisable"),"later-cleaner")?;
            assert!(folder_integration_needs_attention(exe));
            restore_value(&SavedValue{key:owned_root.into(),name:Some("LegacyDisable".into()),kind:None,data:None})?;
            assert!(!owns_folder_integration(Path::new(r"C:\Other\ZSDesk.exe")));
            let owned_win_e=read_checked(WIN_E_COMMAND,None)?;
            write_string(WIN_E_COMMAND,None,"independent-win-e-handler")?;
            assert!(owns_folder_integration(exe),"folder ownership must not depend on Win+E");
            restore_value(&owned_win_e)?;
            for value in planned_values(Integration::Default,exe).into_iter().filter(|value|builtin_key(&value.key)){
                assert_eq!(read_checked(&value.key,value.name.as_deref())?,value);
            }
            assert_eq!(read_checked(folder,None)?,folder_before);
            assert!(restore_owned(exe,&backup_path)?);
            assert!(!owns_folder_integration(exe));
            assert_eq!(read_checked(owned_root,Some("LegacyDisable"))?,original_private_hidden);
            for value in &original{assert_eq!(read_checked(&value.key,value.name.as_deref())?,*value,"raw original value/type was lost");}
            restore(&backup_path)?;

            // A v1 backup predates canonical verbs. Upgrade appends their exact
            // existing handlers and hidden values without replacing old records.
            let (legacy_values,legacy_keys)=plan(Integration::Default,exe);
            let legacy_values:Vec<_>=legacy_values.into_iter().filter(|(key,_,_)|!builtin_key(key)).collect();
            let mut legacy=Backup{version:1,..Default::default()};
            for (key,name,_) in &legacy_values{legacy.values.push(read_checked(key,*name)?);}
            legacy.created_keys=legacy_keys.into_iter().filter(|key|!builtin_key(key)&&!key_exists(key)).collect();
            save_backup(&backup_path,&legacy)?;
            for (key,name,text) in &legacy_values{write_string(key,*name,text)?;}
            assert!(upgrade_default(exe,&backup_path)?);
            let upgraded:Backup=serde_json::from_slice(&std::fs::read(&backup_path)?)?;
            assert_eq!(upgraded.version,2);assert_eq!(&upgraded.values[..legacy.values.len()],legacy.values.as_slice());
            assert!(original.iter().all(|value|upgraded.values.contains(value)));
            assert!(!upgrade_default(exe,&backup_path)?);
            restore(&backup_path)?;
            for value in &original{assert_eq!(read_checked(&value.key,value.name.as_deref())?,*value);}

            apply(Integration::Default,exe,&backup_path)?;
            let changed_command=r"Software\Classes\Directory\shell\open\command";
            let changed_root=r"Software\Classes\Directory\shell\open";
            write_string(changed_command,None,r#""C:\Later Manager\manager.exe" "%1""#)?;
            write_string(changed_root,Some("LegacyDisable"),"later-owner")?;
            let foreign_command=read_checked(changed_command,None)?;let foreign_hidden=read_checked(changed_root,Some("LegacyDisable"))?;
            assert!(!owns_folder_integration(exe));
            let delegate_only=r"Software\Classes\Drive\shell\explore\command";
            assert!(folder_integration_needs_attention(exe));
            write_string(delegate_only,Some("DelegateExecute"),"{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}")?;
            let foreign_delegate=read_checked(delegate_only,Some("DelegateExecute"))?;
            let foreign_key=r"Software\Classes\Directory\shell\ZSDesk\later-extension";write_string(foreign_key,Some("Data"),"keep")?;
            let backup_bytes=std::fs::read(&backup_path)?;
            assert!(apply(Integration::Default,exe,&backup_path).is_err(),"reapplication overwrote a later manager");
            assert_eq!(std::fs::read(&backup_path)?,backup_bytes);
            assert!(!upgrade_default(exe,&backup_path)?,"upgrade must not reclaim already-recorded foreign handlers");
            assert!(restore_owned(exe,&backup_path)?);restore(&backup_path)?;
            assert_eq!(read_checked(changed_command,None)?,foreign_command);assert_eq!(read_checked(changed_root,Some("LegacyDisable"))?,foreign_hidden);
            assert_eq!(read_checked(delegate_only,Some("DelegateExecute"))?,foreign_delegate);
            assert_eq!(read_string(foreign_key,Some("Data")).as_deref(),Some("keep"));
            assert_eq!(read_checked(folder,None)?,folder_before);
            Ok(())
        })();
        unsafe{RegOverridePredefKey(HKEY_CURRENT_USER,None)}.ok()?;sandbox.active=false;
        let key=sandbox.key.clone();drop(sandbox);let cleaned=!key_exists(&key);
        let report=serde_json::json!({"state":if result.is_ok()&&cleaned{"passed"}else{"failed"},"process_local_registry_override":true,
            "registry_cleanup":cleaned,"shell_commands_invoked":false,"error":result.as_ref().err().map(|error|error.to_string())});
        std::fs::write(output.join("canonical-verbs.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
        result?;assert!(cleaned);Ok(())
    }
}
