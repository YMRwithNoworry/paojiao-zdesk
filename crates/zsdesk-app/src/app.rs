use std::{ffi::OsString, path::PathBuf};
use anyhow::{bail, Context, Result};
use zsdesk_core::model::explorer::{ExplorerSession, ExplorerTab, ShellLocation};
use crate::{desktop::{self, DesktopCommand}, storage, window};

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Explorer,
    Desktop(DesktopCommand),
    /// Recovery watchdog for desktop icons (internal).
    Guard(u32),
    ImportTencent,
}

impl Default for Mode { fn default() -> Self { Self::Explorer } }

#[derive(Default)]
struct Options {
    profile: Option<PathBuf>,
    classic: bool,
    target: Option<OsString>,
    select: bool,
    mode: Mode,
    import_source: Option<PathBuf>,
    import_report: Option<PathBuf>,
    apply_import: bool,
    diagnostics_main: Option<PathBuf>,
    uninstall_integration: bool,
    routing_apply: bool,
}

fn parse(args: impl IntoIterator<Item=OsString>) -> Result<Options> {
    let mut opts = Options::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--uninstall-integration" { opts.uninstall_integration = true; continue; }
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
        if arg == "--shell-open" { continue; }
        if arg == "--desktop" { if opts.mode == Mode::ImportTencent { bail!("导入命令不能启动桌面界面"); } if opts.mode == Mode::Explorer { opts.mode = Mode::Desktop(DesktopCommand::Show); } continue; }
        if arg == "--desktop-guard" {
            if opts.mode == Mode::ImportTencent { bail!("导入命令不能启动桌面守护器"); }
            let pid = iter.next().context("缺少进程号")?.to_string_lossy().parse().context("进程号无效")?;
            opts.mode = Mode::Guard(pid);
            continue;
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
    if opts.select && opts.target.is_none() { bail!("缺少定位目标"); }
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

/// Target of a `--shell-open` launch, so `main` can fall back to Explorer.
pub fn shell_open_target() -> Option<OsString> {
    let mut args = std::env::args_os().skip_while(|a| a != "--shell-open");
    args.next()?;
    args.next()
}

/// Used before parsing succeeds so malformed background commands never show UI.
pub fn is_import_cli() -> bool {
    std::env::args_os().skip(1).any(|arg| arg == "--import-tencent" || arg == "--import-report" || arg == "--apply-import" || arg == "--diagnostics" || arg == "--diagnostics-main" || arg == "--build-info" || arg == "--uninstall-integration" || arg == "--routing-apply" || arg == "--win-e-agent" || arg == "--win-e-agent-stop")
}

pub fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    if let Some(mode) = args.next() {
        if mode == "--win-e-agent" {
            if args.next().is_some() { bail!("--win-e-agent 不能与其它参数混用"); }
            return crate::win_e_agent::run();
        }
        if mode == "--win-e-agent-stop" {
            if args.next().is_some() { bail!("--win-e-agent-stop 不能与其它参数混用"); }
            let _ = crate::win_e_agent::stop()?;
            return Ok(());
        }
    }
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--build-info") {
        if std::env::args_os().count() != 2 { bail!("--build-info 不能与其他参数混用"); }
        println!("{}", serde_json::json!({"product":"ZSDesk", "version":env!("CARGO_PKG_VERSION"),
            "edition":crate::edition::key(), "edition_name":crate::edition::name(),
            "network":crate::edition::NETWORK, "desktop":crate::edition::DESKTOP, "platform":"windows-x64"}));
        return Ok(());
    }
    let opts = parse(std::env::args_os().skip(1))?;
    let config = std::path::absolute(opts.profile.map(|p| p.join("explorer-session.json")).unwrap_or_else(storage::config_path))?;
    let desktop_config = config.with_file_name("desktop-fences.json");
    if opts.routing_apply {
        println!("{}", crate::routing_control::apply_owned_settings(&config)?);
        return Ok(());
    }
    if opts.uninstall_integration {
        let exe = std::env::current_exe()?;
        crate::routing_control::uninstall_owned(&config, &exe)?;
        zsdesk_windows::integration::restore_owned(&exe, &config.with_file_name("takeover-backup.json"))?;
        let _ = zsdesk_windows::desktop::set_win_e_agent_autostart(None);
        let _ = crate::win_e_agent::stop()?;
        return Ok(());
    }
    if let Some(report)=opts.diagnostics_main{return crate::instance::diagnose(&config,&std::path::absolute(report)?);}
    match opts.mode {
        Mode::Desktop(_) | Mode::Guard(_) | Mode::ImportTencent if !crate::edition::DESKTOP => {
            bail!("ZSDesk {} 不包含桌面格子，请使用桌面版或全量版", crate::edition::name())
        }
        Mode::Desktop(command) => return desktop::run(desktop_config, config, command),
        Mode::Guard(pid) => return desktop::guard(desktop_config, pid),
        Mode::ImportTencent => return desktop::migration::run(
            opts.import_source.as_deref().context("缺少腾讯布局路径")?, &desktop_config,
            opts.import_report.as_deref().context("缺少导入报告路径")?, opts.apply_import),
        Mode::Explorer => {}
    }
    let mut requested = None;
    let mut selection = None;
    if let Some(argument) = opts.target {
        let text = argument.to_string_lossy();
        let location = if text.eq_ignore_ascii_case("shell:MyComputerFolder") {
            ShellLocation::ThisPc
        } else if text.starts_with("shell:") || text.starts_with("::{") {
            ShellLocation::Shell { parsing_name: text.into_owned(), title: "Shell".into() }
        } else {
            let path = std::path::absolute(PathBuf::from(&argument))?;
            if opts.select || path.is_file() {
                selection = Some(path.file_name().context("该项目没有父目录")?.to_owned());
                ShellLocation::Path(path.parent().context("该项目没有父目录")?.into())
            } else { ShellLocation::Path(path) }
        };
        requested = Some(location);
    }
    let Some(_instance) = crate::instance::enter(&config, requested.as_ref(), selection.as_deref())? else { return Ok(()) };
    let loaded = if config.try_exists()? { Some(storage::load(&config)?) } else { None };
    let fresh = loaded.is_none();
    let mut session = loaded.unwrap_or_default();
    session.normalize();
    if let Some(location) = requested { activate(&mut session, location, fresh); }
    window::run(session, config, selection, !opts.classic)
}

fn activate(session: &mut ExplorerSession, target: ShellLocation, fresh: bool) {
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
        assert_eq!(parse(["--desktop-guard", "42"].map(OsString::from)).unwrap().mode, Mode::Guard(42));
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
