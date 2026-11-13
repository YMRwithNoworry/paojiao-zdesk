use std::{ffi::OsString, path::PathBuf};
use anyhow::{bail, Context, Result};
use zsdesk_core::model::explorer::{ExplorerSession, ExplorerTab, ShellLocation};
use crate::{desktop::{self, DesktopCommand}, storage, window};

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Explorer,
    Desktop(DesktopCommand),
    /// Recovery watchdog for desktop icons (internal).
    Guard(u32,Option<u32>),
    ImportTencent,
    QuickSearch(crate::quick_search::Action),
}

impl Default for Mode { fn default() -> Self { Self::Explorer } }

#[derive(Default)]
struct Options {
    profile: Option<PathBuf>,
    classic: bool,
    target: Option<OsString>,
    window_location:Option<ShellLocation>,
    shell_open:bool,
    select: bool,
    mode: Mode,
    import_source: Option<PathBuf>,
    import_report: Option<PathBuf>,
    apply_import: bool,
    diagnostics_main: Option<PathBuf>,
    uninstall_integration: bool,
    routing_apply: bool,
    integration_refresh: bool,
    integration_enable: bool,
    win_e: bool,
    new_window: bool,
    focus_search:bool,
    search_settings:bool,
}

fn parse(args: impl IntoIterator<Item=OsString>) -> Result<Options> {
    let mut opts = Options::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg=="--window-location"{
            let value=iter.next().context("缺少窗口位置数据")?;
            let location=decode_window_location(&value)?;
            if opts.window_location.replace(location).is_some(){bail!("窗口位置只能指定一次");}
            continue;
        }
        if arg=="--focus-search"{opts.focus_search=true;continue;}
        if arg=="--search-settings"{opts.focus_search=true;opts.search_settings=true;continue;}
        if arg=="--integration-enable"{opts.integration_enable=true;continue;}
        if arg == "--new-window" { opts.new_window=true; continue; }
        let quick = match arg.to_str() {
            Some("--quick-search") => Some(crate::quick_search::Action::Open),
            Some("--quick-search-settings") => Some(crate::quick_search::Action::Settings),
            Some("--background") => Some(crate::quick_search::Action::Resident),
            Some("--background-stop") => Some(crate::quick_search::Action::Stop),
            Some("--background-status") => Some(crate::quick_search::Action::Status),
            Some("--background-enable") => Some(crate::quick_search::Action::EnableAtLogin),
            Some("--background-disable") => Some(crate::quick_search::Action::Disable),
            _ => None,
        };
        if let Some(action) = quick {
            if opts.mode != Mode::Explorer { bail!("快速搜索不能与其他启动模式混用"); }
            opts.mode = Mode::QuickSearch(action); continue;
        }
        if arg == "--uninstall-integration" { opts.uninstall_integration = true; continue; }
        if arg == "--integration-refresh" {opts.integration_refresh=true;continue;}
        if arg == "--win-e" {opts.win_e=true;continue;}
        if arg == "--routing-apply" { opts.routing_apply = true; continue; }
        if arg=="--diagnostics-main"{opts.diagnostics_main=Some(iter.next().context("缺少诊断报告路径")?.into());continue;}
        if arg == "--import-tencent" {
            if opts.mode != Mode::Explorer || opts.import_source.is_some() { bail!("导入命令不能与其他启动模式混用"); }
            let value = iter.next().context("缺少腾讯布局路径；使用 - 自动查找")?;
            if value.to_string_lossy().starts_with("--") { bail!("缺少腾讯布局路径；使用 - 自动查找"); }
            opts.import_source = Some(value.into()); opts.mode = Mode::ImportTencent; continue;
        }
        if arg == "--import-report" {
            let value = iter.next().context("缺少导入报告路径")?;
            if value.to_string_lossy().starts_with("--") { bail!("缺少导入报告路径"); }
            if opts.import_report.replace(value.into()).is_some() { bail!("导入报告路径只能指定一次"); }
            continue;
        }
        if arg == "--apply-import" { opts.apply_import = true; continue; }
        if arg == "--classic-menu" { opts.classic = true; continue; }
        if arg == "--profile-dir" { opts.profile = Some(PathBuf::from(iter.next().context("缺少配置目录")?)); continue; }
        if arg == "--select" { opts.select = true; continue; }
        // Registered by system integration: behaves like a plain path argument.
        if arg == "--shell-open" {opts.shell_open=true; continue; }
        if arg == "--desktop" { if matches!(opts.mode, Mode::ImportTencent | Mode::QuickSearch(_)) { bail!("不能混用桌面和其他启动模式"); } if opts.mode == Mode::Explorer { opts.mode = Mode::Desktop(DesktopCommand::Show); } continue; }
        if arg == "--desktop-guard" {
            if matches!(opts.mode, Mode::ImportTencent | Mode::QuickSearch(_)) { bail!("不能混用守护器和其他启动模式"); }
            let pid = iter.next().context("缺少进程号")?.to_string_lossy().parse().context("进程号无效")?;
            opts.mode = Mode::Guard(pid,None);
            continue;
        }
        if arg=="--guard-thread"{
            let thread=iter.next().context("缺少守护线程号")?.to_string_lossy().parse().context("线程号无效")?;
            if let Mode::Guard(_,slot)=&mut opts.mode{*slot=Some(thread);}else{bail!("守护线程参数只能用于恢复进程");}continue;
        }
        if let Mode::Desktop(command) = &mut opts.mode {
            if arg == "--organize" { *command = DesktopCommand::Organize; continue; }
            if arg == "--restore" { *command = DesktopCommand::Restore; continue; }
            if arg == "--new-fence" { *command = DesktopCommand::NewFence; continue; }
            if arg == "--quit" { *command = DesktopCommand::Quit; continue; }
            if arg == "--tray" { *command = DesktopCommand::Tray; continue; }
            if arg == "--diagnostics" { *command = DesktopCommand::Diagnostics(PathBuf::from(iter.next().context("缺少诊断输出路径")?)); continue; }
            if arg == "--map" { *command = DesktopCommand::Map(std::path::absolute(PathBuf::from(iter.next().context("缺少文件夹")?))?); continue; }
        }
        let target = if arg == "--this-pc" { OsString::from("shell:MyComputerFolder") } else { arg };
        if opts.target.replace(target).is_some() { bail!("只能指定一个文件或目录"); }
    }
    if opts.window_location.is_some()&&(!opts.new_window||opts.mode!=Mode::Explorer||opts.target.is_some()||opts.select||opts.shell_open
        ||opts.integration_enable||opts.integration_refresh||opts.uninstall_integration||opts.routing_apply||opts.win_e
        ||opts.diagnostics_main.is_some()||opts.import_source.is_some()||opts.import_report.is_some()||opts.apply_import||opts.focus_search){
        bail!("窗口位置数据只能用于独立文件窗口，不能与定位、集成或其他启动模式混用");
    }
    if opts.select && opts.target.is_none() { bail!("缺少定位目标"); }
    if opts.integration_enable&&(opts.mode!=Mode::Explorer||opts.target.is_some()||opts.select||opts.classic||opts.integration_refresh||opts.uninstall_integration||opts.routing_apply||opts.diagnostics_main.is_some()||opts.new_window||opts.win_e){bail!("启用系统集成不能与其他操作混用");}
    if opts.win_e && (opts.mode!=Mode::Explorer || opts.target.is_some() || opts.select || opts.classic || opts.integration_refresh || opts.uninstall_integration || opts.routing_apply || opts.diagnostics_main.is_some()) {bail!("Win+E 启动不能与其他操作混用");}
    if opts.integration_refresh && (opts.mode!=Mode::Explorer || opts.target.is_some() || opts.select || opts.classic || opts.uninstall_integration || opts.routing_apply || opts.diagnostics_main.is_some()) {bail!("集成修复不能与其他操作混用");}
    if matches!(opts.mode, Mode::QuickSearch(_)) && (opts.target.is_some() || opts.select || opts.classic) { bail!("快速搜索启动不能附带文件浏览参数"); }
    if opts.uninstall_integration && (opts.mode != Mode::Explorer || opts.target.is_some() || opts.select || opts.classic || opts.diagnostics_main.is_some()) {
        bail!("卸载恢复不能与文件浏览或其他启动模式混用");
    }
    if opts.routing_apply && (opts.uninstall_integration || opts.mode != Mode::Explorer || opts.target.is_some() || opts.select || opts.classic || opts.diagnostics_main.is_some()) {
        bail!("后台文件位置设置不能与其他启动模式混用");
    }
    if opts.diagnostics_main.is_some() && (opts.mode!=Mode::Explorer || opts.target.is_some() || opts.select){bail!("窗口诊断不能与其他启动模式混用");}
    if opts.mode == Mode::ImportTencent {
        if opts.import_report.is_none() { bail!("导入命令必须指定 --import-report 报告路径"); }
        if opts.target.is_some() || opts.select || opts.classic { bail!("导入命令不能与文件浏览参数混用"); }
    } else if opts.import_report.is_some() || opts.apply_import {
        bail!("--import-report 和 --apply-import 必须与 --import-tencent 配合使用");
    }
    Ok(opts)
}

const MAX_WINDOW_LOCATION_JSON:usize=96*1024;
fn validate_window_location(location:&ShellLocation)->Result<()>{
    use std::os::windows::ffi::OsStrExt;
    match location{
        ShellLocation::ThisPc=>{},
        ShellLocation::Path(path)=>{
            let units:Vec<_>=path.as_os_str().encode_wide().collect();
            if !path.is_absolute()||units.contains(&0)||units.len()>32767{bail!("窗口文件夹路径无效");}
        },
        ShellLocation::Shell{parsing_name,title,pidl}=>{
            if parsing_name.contains('\0')||title.contains('\0')||parsing_name.encode_utf16().count()>32767||title.len()>4096{bail!("Shell 窗口位置名称无效");}
            if let Some(identity)=pidl{if !zsdesk_windows::shell::pidl::valid(identity){bail!("Shell 窗口标识无效或超过 64 KiB");}}
            if parsing_name.trim().is_empty()&&pidl.is_none(){bail!("Shell 窗口位置缺少标识");}
        },
        ShellLocation::Search{..}=>bail!("独立窗口位置不能携带临时搜索状态"),
    }
    Ok(())
}
fn decode_window_location(value:&std::ffi::OsStr)->Result<ShellLocation>{
    let value=value.to_str().context("窗口位置数据不是有效 Unicode")?;
    if value.len()>MAX_WINDOW_LOCATION_JSON{bail!("窗口位置数据过长");}
    let location=serde_json::from_str(value).context("窗口位置数据无效")?;
    validate_window_location(&location)?;Ok(location)
}
/// Preserve opaque namespace identities across the existing runtime ingress.
/// Ordinary filesystem arguments stay lossless Windows OsStrings.
pub(crate) fn new_window_arguments(profile:&std::path::Path,location:&ShellLocation)->Result<Vec<OsString>>{
    let mut args=vec!["--new-window".into(),"--profile-dir".into(),profile.as_os_str().to_owned()];
    match location{
        ShellLocation::Shell{..}=>{
            validate_window_location(location)?;
            let value=serde_json::to_string(location)?;
            if value.len()>MAX_WINDOW_LOCATION_JSON{bail!("窗口位置数据过长");}
            args.extend(["--window-location".into(),value.into()]);
        },
        ShellLocation::Path(path)|ShellLocation::Search{scope:Some(path),..}=>args.push(path.as_os_str().to_owned()),
        _=>args.push("--this-pc".into()),
    }
    Ok(args)
}
pub(crate) fn window_command_line_fits(executable:&std::path::Path,args:&[OsString])->bool{
    use std::os::windows::ffi::OsStrExt;
    // Quote every argument for a conservative UTF-16 upper bound, including
    // escaped JSON quotes and trailing backslashes. Include the final NUL.
    let mut total=executable.as_os_str().encode_wide().count()+3;
    for argument in args{
        total=total.saturating_add(3);let mut slashes=0usize;
        for unit in argument.encode_wide(){
            if unit==0{return false;}
            if unit==92{slashes+=1;continue;}
            total=total.saturating_add(if unit==34{slashes.saturating_mul(2)+2}else{slashes+1});slashes=0;
        }
        total=total.saturating_add(slashes.saturating_mul(2));
        if total>32767{return false;}
    }
    total<=32767
}

/// Target of a `--shell-open` launch, so `main` can fall back to Explorer.
pub fn shell_open_target() -> Option<OsString> {
    let mut args = std::env::args_os().skip_while(|a| a != "--shell-open");
    args.next()?;
    args.next()
}

/// Used before parsing succeeds so malformed background commands never show UI.
pub fn is_import_cli() -> bool {
    std::env::args_os().skip(1).any(|arg| arg == "--import-tencent" || arg == "--import-report" || arg == "--apply-import" || arg == "--diagnostics" || arg == "--diagnostics-main" || arg == "--build-info" || arg == "--uninstall-integration" || arg == "--routing-apply" || arg == "--win-e-agent" || arg == "--win-e-agent-stop"
        || arg == "--background-stop" || arg == "--background-status" || arg == "--background-enable" || arg == "--background-disable" || arg == "--integration-refresh" || arg == "--integration-enable")
}

pub fn is_background_mode() -> bool { std::env::args_os().any(|arg| arg == "--background") }
pub fn is_win_e() -> bool { std::env::args_os().skip(1).any(|arg| arg == "--win-e") }

pub fn open_original_explorer(target: OsString) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStringExt;
    let mut directory = vec![0u16; 32768];
    let length = unsafe { windows::Win32::System::SystemInformation::GetWindowsDirectoryW(Some(&mut directory)) } as usize;
    if length == 0 || length >= directory.len() { return Err(std::io::Error::last_os_error()); }
    let executable = PathBuf::from(OsString::from_wide(&directory[..length])).join("explorer.exe");
    std::process::Command::new(executable).arg(target).spawn().map(|_| ())
}

pub fn run() -> Result<()> {
    run_arguments(std::env::args_os().skip(1).collect(), false)
}

fn managed(options: &Options) -> bool {
    !options.integration_enable && !options.integration_refresh && !options.routing_apply && !options.uninstall_integration
        && options.diagnostics_main.is_none() && match &options.mode {
            Mode::Explorer => true,
            Mode::Desktop(command) => !matches!(command, DesktopCommand::Quit | DesktopCommand::Diagnostics(_)),
            Mode::QuickSearch(action) => matches!(action, crate::quick_search::Action::Open | crate::quick_search::Action::Settings | crate::quick_search::Action::Resident),
            _ => false,
        }
}

pub(crate) fn runtime_profile(args: &[OsString]) -> Result<Option<PathBuf>> {
    let options = parse(edition_arguments(args.to_vec())?)?;
    if !managed(&options) { return Ok(None); }
    Ok(Some(std::path::absolute(options.profile.map(|p| p.join("explorer-session.json")).unwrap_or_else(storage::config_path))?))
}

fn edition_arguments(mut args: Vec<OsString>) -> Result<Vec<OsString>> {
    if !crate::edition::DESKTOP_ONLY { return Ok(args); }
    let options = parse(args.iter().cloned())?;
    if options.uninstall_integration { return Ok(args); }
    if options.integration_enable || options.integration_refresh || options.routing_apply || options.win_e || options.focus_search || options.diagnostics_main.is_some()
        || options.new_window || options.target.is_some() || options.select || matches!(options.mode, Mode::QuickSearch(_)) {
        bail!("桌面盒子独立版仅提供桌面格子；文件管理与搜索请使用 ZSDesk 文件管理版本");
    }
    if options.mode == Mode::Explorer { args.push("--desktop".into()); }
    Ok(args)
}

pub(crate) fn run_arguments(args: Vec<OsString>, in_host: bool) -> Result<()> {
    if args.first().is_some_and(|arg| arg == "--win-e-agent") {
        if args.len() != 1 { bail!("--win-e-agent 不能与其它参数混用"); }
        return crate::win_e_agent::run();
    }
    if args.first().is_some_and(|arg| arg == "--win-e-agent-stop") {
        if args.len() != 1 { bail!("--win-e-agent-stop 不能与其它参数混用"); }
        let _ = crate::win_e_agent::stop()?;
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--build-info") {
        if args.len() != 1 { bail!("--build-info 不能与其他参数混用"); }
        println!("{}", serde_json::json!({"product":"ZSDesk", "version":env!("CARGO_PKG_VERSION"),
            "edition":crate::edition::key(), "edition_name":crate::edition::name(),
            "network":crate::edition::NETWORK, "desktop":crate::edition::DESKTOP, "themed":crate::edition::THEMED,"desktop_only":crate::edition::DESKTOP_ONLY,"platform":"windows-x64"}));
        return Ok(());
    }
    let args=edition_arguments(args)?;
    let app_id = crate::edition::application_id();
    let app_id: Vec<u16> = app_id.encode_utf16().chain(Some(0)).collect();
    let _ = unsafe { windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(windows::core::PCWSTR(app_id.as_ptr())) };
    let opts = parse(args.iter().cloned())?;
    let config = std::path::absolute(opts.profile.as_ref().map(|p| p.join("explorer-session.json")).unwrap_or_else(storage::config_path))?;
    if !in_host&&managed(&opts){return crate::runtime::run(config,args);}
    let desktop_config = config.with_file_name("desktop-fences.json");
    if opts.integration_enable{
        let exe=std::env::current_exe()?;
        zsdesk_windows::integration::apply(zsdesk_windows::integration::Integration::Default,&exe,&config.with_file_name("takeover-backup.json"))?;
        println!("{}",serde_json::json!({"enabled":true,"win_e":zsdesk_windows::integration::win_e_enabled(&exe),"this_pc":zsdesk_windows::integration::this_pc_enabled(&exe),"folder_takeover":zsdesk_windows::integration::owns_folder_integration(&exe)}));return Ok(());
    }
    if opts.integration_refresh {
        let exe=std::env::current_exe()?;
        let changed=zsdesk_windows::integration::upgrade_default(&exe,&config.with_file_name("takeover-backup.json"))?;
        println!("{}",serde_json::json!({"updated":changed,"win_e":zsdesk_windows::integration::win_e_enabled(&exe),"folder_takeover":zsdesk_windows::integration::owns_folder_integration(&exe)}));
        return Ok(());
    }
    if opts.routing_apply {
        println!("{}", crate::routing_control::apply_owned_settings(&config)?);
        return Ok(());
    }
    if opts.uninstall_integration {
        let exe = std::env::current_exe()?;
        desktop::uninstall_owned_startup(&desktop_config, &exe)?;
        crate::quick_search::control::uninstall_owned(&config, &exe)?;
        crate::routing_control::uninstall_owned(&config, &exe)?;
        zsdesk_windows::integration::restore_owned(&exe, &config.with_file_name("takeover-backup.json"))?;
        let _ = zsdesk_windows::desktop::set_win_e_agent_autostart(None);
        let _ = crate::win_e_agent::stop()?;
        return Ok(());
    }
    if let Some(report)=opts.diagnostics_main{return crate::instance::diagnose(&config,&std::path::absolute(report)?);}
    match opts.mode {
        Mode::Desktop(_) | Mode::Guard(..) | Mode::ImportTencent if !crate::edition::DESKTOP => {
            bail!("ZSDesk {} 不包含桌面格子，请使用桌面版或全量版", crate::edition::name())
        }
        Mode::Desktop(command) => return desktop::run(desktop_config, config, command),
        Mode::Guard(pid,thread) => return desktop::guard(desktop_config, pid,thread),
        Mode::ImportTencent => return desktop::migration::run(
            opts.import_source.as_deref().context("缺少腾讯布局路径")?, &desktop_config,
            opts.import_report.as_deref().context("缺少导入报告路径")?, opts.apply_import),
        Mode::QuickSearch(action) => return crate::quick_search::run(config, action),
        Mode::Explorer => {}
    }
    let mut requested = opts.window_location;
    let mut selection = None;
    if let Some(argument) = opts.target {
        let text = argument.to_string_lossy();
        let location = if text.eq_ignore_ascii_case("shell:MyComputerFolder") {
            ShellLocation::ThisPc
        } else if text.starts_with("shell:") || text.starts_with("::{") {
            ShellLocation::Shell { pidl:None, parsing_name: text.into_owned(), title: "Shell".into() }
        } else {
            let path = std::path::absolute(PathBuf::from(&argument))?;
            if opts.select || path.is_file() {
                selection = Some(path.file_name().context("该项目没有父目录")?.to_owned());
                ShellLocation::Path(path.parent().context("该项目没有父目录")?.into())
            } else { ShellLocation::Path(path) }
        };
        requested = Some(location);
    }
    if opts.focus_search&&crate::instance::focus_search(&config,opts.search_settings)?{return Ok(());}
    let _instance = if opts.new_window { None } else {
        let Some(instance)=crate::instance::enter(&config, requested.as_ref(), selection.as_deref())? else {return Ok(());}; Some(instance)
    };
    let loaded = if config.try_exists()? { Some(storage::load(&config)?) } else { None };
    let fresh = loaded.is_none() || opts.new_window;
    let mut session = loaded.unwrap_or_else(storage::default_session);
    session.normalize();
    if let Some(location) = requested { activate(&mut session, location, fresh); }
    else {session.apply_startup_location();}
    window::run(session, config, selection, !opts.classic,opts.new_window,opts.focus_search.then_some(opts.search_settings))
}

fn activate(session: &mut ExplorerSession, target: ShellLocation, fresh: bool) {
    if fresh { session.tabs.clear(); }
    if let Some(index) = session.tabs.iter().position(|t| t.current() == &target) {
        session.active_tab = index;
    } else {
        if fresh { session.tabs.clear(); }
        let id = session.take_id();
        session.tabs.push(ExplorerTab::new(id, target));
        session.active_tab = session.tabs.len() - 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]fn typed_window_location_preserves_namespace_identity(){
        let location=ShellLocation::Shell{parsing_name:String::new(),title:"设备存储 中文".into(),pidl:Some(vec![4,0,7,9,0,0])};
        let args=new_window_arguments(std::path::Path::new(r"E:\配置 空格"),&location).unwrap();
        let options=parse(args.clone()).unwrap();
        assert!(options.new_window&&options.target.is_none()&&!options.select);
        assert_eq!(options.window_location,Some(location));
        assert!(window_command_line_fits(std::path::Path::new(r"C:\Program Files\ZSDesk\ZSDesk.exe"),&args));
    }
    #[test]fn typed_window_location_rejects_mixed_modes_and_invalid_identity(){
        let location=ShellLocation::Shell{parsing_name:String::new(),title:"设备".into(),pidl:Some(vec![0,0])};
        let json=serde_json::to_string(&location).unwrap();
        assert!(parse(vec!["--window-location".into(),json.clone().into()]).is_err());
        for conflict in ["--select","--shell-open","--this-pc","--desktop","--background","--integration-enable","--integration-refresh","--uninstall-integration","--routing-apply","--win-e","--focus-search",r"C:\other"]{
            assert!(parse(vec!["--new-window".into(),"--window-location".into(),json.clone().into(),conflict.into()]).is_err(),"accepted {conflict}");
        }
        assert!(parse(vec!["--new-window".into(),"--window-location".into(),json.clone().into(),"--window-location".into(),json.into()]).is_err());
        for invalid in [
            ShellLocation::Shell{parsing_name:String::new(),title:String::new(),pidl:None},
            ShellLocation::Shell{parsing_name:"shell:Desktop".into(),title:String::new(),pidl:Some(vec![9,0])},
            ShellLocation::Path("relative".into()),ShellLocation::Search{query:"name".into(),scope:None},
        ]{assert!(decode_window_location(std::ffi::OsStr::new(&serde_json::to_string(&invalid).unwrap())).is_err());}
        assert!(decode_window_location(std::ffi::OsStr::new(&"x".repeat(MAX_WINDOW_LOCATION_JSON+1))).is_err());
    }
    #[test]fn typed_window_location_respects_packet_and_command_line_bounds(){
        let mut identity=vec![2,0].repeat(12_000);identity.extend([0,0]);
        let location=ShellLocation::Shell{parsing_name:String::new(),title:"设备".into(),pidl:Some(identity)};
        let args=new_window_arguments(std::path::Path::new(r"E:\profile"),&location).unwrap();
        #[derive(serde::Serialize)]struct Envelope<'a>{args:&'a[OsString]}
        assert!(serde_json::to_vec(&Envelope{args:&args}).unwrap().len()>128*1024);
        assert!(!window_command_line_fits(std::path::Path::new(r"C:\ZSDesk.exe"),&args));
        assert!(!window_command_line_fits(std::path::Path::new(r"C:\ZSDesk.exe"),&["bad\0path".into()]));
        let ordinary=new_window_arguments(std::path::Path::new(r"E:\profile"),&ShellLocation::Path(r"E:\中文 空格".into())).unwrap();
        assert_eq!(parse(ordinary).unwrap().target,Some(r"E:\中文 空格".into()));
    }
    #[test]fn standalone_box_routes_to_desktop_and_keeps_a_separate_identity(){
        if crate::edition::DESKTOP_ONLY{
            let arguments=edition_arguments(vec![]).unwrap();assert!(matches!(parse(arguments).unwrap().mode,Mode::Desktop(DesktopCommand::Show)));
            assert_eq!(crate::edition::key(),"desktop-box");assert_eq!(crate::edition::profile_name(),"ZSDesk-DesktopBox");
            assert!(!crate::edition::NETWORK&&!crate::edition::THEMED);
            assert_ne!(crate::edition::desktop_startup_value(),"ZSDesk.Desktop");
            for argument in ["--focus-search","--win-e","--integration-enable","--background",r"E:\folder"]{assert!(edition_arguments(vec![argument.into()]).is_err());}
        }else{assert!(edition_arguments(vec![]).unwrap().is_empty());}
    }
    #[test] fn win_e_starts_the_manager_without_a_synthetic_folder() {
        let options=parse(["--win-e"].map(OsString::from)).unwrap();
        assert!(options.win_e && options.target.is_none() && options.mode==Mode::Explorer);
        for args in [["--win-e","--desktop"],["--win-e","--background"],["--win-e","--integration-refresh"]] {assert!(parse(args.map(OsString::from)).is_err());}
    }
    #[test]
    fn reveal_preserves_other_tabs_and_reuses_open_location() {
        let mut s = ExplorerSession::default();
        let a = ShellLocation::Path("C:/a".into());
        let b = ShellLocation::Path("C:/b".into());
        activate(&mut s, a.clone(), true);
        activate(&mut s, b, false);
        activate(&mut s, a, false);
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(s.active_tab, 0);
    }
    #[test]
    fn selection_requires_target_and_keeps_unicode_names() {
        assert!(parse([OsString::from("--select")]).is_err());
        let opts = parse(["--select", "--classic-menu", r"C:\工作 文件\图纸.pdf"].map(OsString::from)).unwrap();
        assert!(opts.select && opts.classic);
        assert_eq!(opts.target.unwrap(), r"C:\工作 文件\图纸.pdf");
    }
    #[test]
    fn desktop_modes_parse() {
        assert_eq!(parse(["--desktop"].map(OsString::from)).unwrap().mode, Mode::Desktop(DesktopCommand::Show));
        assert_eq!(parse(["--desktop", "--organize"].map(OsString::from)).unwrap().mode, Mode::Desktop(DesktopCommand::Organize));
        assert_eq!(parse(["--desktop-guard", "42"].map(OsString::from)).unwrap().mode, Mode::Guard(42,None));
        // Without --desktop, desktop switches are treated as paths.
        assert!(parse(["--organize"].map(OsString::from)).unwrap().target.is_some());
    }
    #[test]
    fn tencent_import_requires_report_and_defaults_to_preview() {
        let preview = parse(["--import-tencent", "-", "--import-report", "report.json", "--profile-dir", "profile"].map(OsString::from)).unwrap();
        assert_eq!(preview.mode, Mode::ImportTencent);
        assert!(!preview.apply_import);
        assert_eq!(preview.import_source.unwrap(), PathBuf::from("-"));
        let apply = parse(["--import-tencent", "layout.dat", "--import-report", "report.json", "--apply-import"].map(OsString::from)).unwrap();
        assert!(apply.apply_import);
        assert!(parse(["--import-tencent", "-"].map(OsString::from)).is_err());
        assert!(parse(["--apply-import"].map(OsString::from)).is_err());
        assert!(parse(["--import-tencent", "-", "--import-report", "--apply-import"].map(OsString::from)).is_err());
        assert!(parse(["--import-tencent", "-", "--import-report", "report.json", "--desktop"].map(OsString::from)).is_err());
    }
}
