//! Explicit terminal launch requests from the address bar and command menu.
use std::{ffi::OsStr, path::{Path, PathBuf}, process::Command};
use std::os::windows::process::CommandExt;
use zsdesk_core::model::explorer::ShellLocation;
use super::WindowState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalKind { PowerShell, WindowsPowerShell, Nushell }

impl TerminalKind {
    fn executable(self) -> &'static str {
        match self { Self::PowerShell => "pwsh.exe", Self::WindowsPowerShell => "powershell.exe", Self::Nushell => "nu.exe" }
    }
    fn label(self) -> &'static str {
        match self { Self::PowerShell => "PowerShell 7", Self::WindowsPowerShell => "Windows PowerShell", Self::Nushell => "Nushell" }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TerminalRequest { kind: TerminalKind, arguments: Vec<&'static str> }

/// Only these executable names and startup switches are terminal requests.
/// No command string, script filename, expansion or redirection is evaluated.
pub(super) fn parse(text: &str) -> Option<Result<TerminalRequest, &'static str>> {
    let mut words = text.split_whitespace();
    let first = words.next()?.to_ascii_lowercase();
    let name = first.strip_suffix(".exe").unwrap_or(&first);
    let kind = match name {
        "pwsh" => TerminalKind::PowerShell, "powershell" => TerminalKind::WindowsPowerShell,
        "nu" | "nushell" => TerminalKind::Nushell, _ => return None,
    };
    let mut arguments = Vec::new();
    for word in words {
        let canonical = match (kind, word.to_ascii_lowercase().as_str()) {
            (TerminalKind::PowerShell | TerminalKind::WindowsPowerShell, "-noprofile") => "-NoProfile",
            (TerminalKind::PowerShell | TerminalKind::WindowsPowerShell, "-nologo") => "-NoLogo",
            (TerminalKind::PowerShell | TerminalKind::WindowsPowerShell, "-noexit") => "-NoExit",
            (TerminalKind::PowerShell | TerminalKind::WindowsPowerShell, "-noninteractive") => "-NonInteractive",
            (TerminalKind::PowerShell | TerminalKind::WindowsPowerShell, "-sta") => "-STA",
            (TerminalKind::PowerShell | TerminalKind::WindowsPowerShell, "-mta") => "-MTA",
            (TerminalKind::Nushell, "--login" | "-l") => "--login",
            (TerminalKind::Nushell, "--no-config-file" | "-n") => "-n",
            (TerminalKind::Nushell, "--no-history") => "--no-history",
            (TerminalKind::Nushell, "--no-std-lib") => "--no-std-lib",
            _ => return Some(Err("地址栏终端仅接受启动开关，不接受命令、脚本或重定向参数")),
        };
        if arguments.len() >= 8 { return Some(Err("终端启动参数过多")); }
        if !arguments.contains(&canonical) { arguments.push(canonical); }
    }
    if arguments.contains(&"-STA") && arguments.contains(&"-MTA") {
        return Some(Err("不能同时指定 -STA 和 -MTA"));
    }
    Some(Ok(TerminalRequest { kind, arguments }))
}

fn working_directory(location: &ShellLocation) -> Option<PathBuf> {
    let path = match location {
        ShellLocation::Path(path) => path,
        ShellLocation::Search { scope: Some(path), .. } => path,
        _ => return None,
    };
    path.is_absolute().then(|| path.clone())
}

fn executable_on_path(name: &OsStr) -> Option<PathBuf> {
    // Do not implicitly execute an identically named file from the browsed
    // directory. Search only explicit absolute PATH entries.
    std::env::var_os("PATH").into_iter().flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .filter(|directory| directory.is_absolute()).map(|directory| directory.join(name)).find(|path| path.is_file())
}

fn resolve(kind: TerminalKind) -> Option<PathBuf> {
    executable_on_path(OsStr::new(kind.executable())).or_else(|| match kind {
        TerminalKind::PowerShell => std::env::var_os("ProgramFiles").map(PathBuf::from)
            .map(|root| root.join(r"PowerShell\7\pwsh.exe")).filter(|path| path.is_file()),
        TerminalKind::WindowsPowerShell => std::env::var_os("SystemRoot").map(PathBuf::from)
            .map(|root| root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe")).filter(|path| path.is_file()),
        TerminalKind::Nushell => None,
    })
}

fn shell_command(executable: &Path, arguments: &[&str], directory: &Path) -> Command {
    let mut command = Command::new(executable);
    command.args(arguments).current_dir(directory)
        .creation_flags(windows::Win32::System::Threading::CREATE_NEW_CONSOLE.0);
    command
}

impl WindowState {
    fn terminal_working_directory(&mut self) -> Option<PathBuf> {
        if self.pending.is_some() { self.show_message("请等待文件夹打开后再启动终端".into()); return None; }
        let directory = working_directory(self.current_location());
        if directory.is_none() { self.show_message("此位置没有文件系统工作目录，请先打开文件夹".into()); }
        directory
    }

    pub(super) fn launch_terminal(&mut self, request: &TerminalRequest) -> bool {
        let Some(directory) = self.terminal_working_directory() else { return false };
        let Some(executable) = resolve(request.kind) else {
            self.show_message(format!("未找到 {}（{}），请安装并加入 PATH", request.kind.label(), request.kind.executable()));
            return false;
        };
        match shell_command(&executable, &request.arguments, &directory).spawn() {
            Ok(_) => true,
            Err(error) => { self.show_message(format!("无法在当前文件夹启动 {}：{error}", request.kind.label())); false }
        }
    }

    pub(super) fn launch_default_terminal(&mut self) {
        let Some(directory) = self.terminal_working_directory() else { return };
        if let Some(wt) = executable_on_path(OsStr::new("wt.exe")) {
            match Command::new(wt).arg("-d").arg(&directory).current_dir(&directory).spawn() {
                Ok(_) => return,
                Err(_) => {}
            }
        }
        let kind = if resolve(TerminalKind::PowerShell).is_some() { TerminalKind::PowerShell } else { TerminalKind::WindowsPowerShell };
        let request = TerminalRequest { kind, arguments: Vec::new() };
        self.launch_terminal(&request);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizes_only_exact_terminal_names_and_safe_switches() {
        let pwsh = parse("  PwSh -NoProfile -NoLogo  ").unwrap().unwrap();
        assert_eq!(pwsh.kind, TerminalKind::PowerShell);
        assert_eq!(pwsh.arguments, ["-NoProfile", "-NoLogo"]);
        assert_eq!(parse("nushell -n --no-history").unwrap().unwrap().kind, TerminalKind::Nushell);
        for text in ["pwsh -Command Get-Date", "powershell -EncodedCommand abc", "nu -c ls", "nu script.nu", "pwsh -NoProfile;exit", "nu > out.txt", "pwsh -STA -MTA"] {
            assert!(parse(text).unwrap().is_err(), "{text}");
        }
        for text in [r"C:\pwsh", r".\pwsh.exe", "pwsh-backup", "cmd /c whoami", ""] { assert!(parse(text).is_none(), "{text}"); }
    }
    #[test]
    fn virtual_locations_do_not_fall_back_to_an_unrelated_directory() {
        assert!(working_directory(&ShellLocation::ThisPc).is_none());
        assert!(working_directory(&ShellLocation::Search { query:"x".into(), scope:None }).is_none());
        let path=PathBuf::from(r"E:\中文 空格 & 资料");
        assert_eq!(working_directory(&ShellLocation::Search { query:"x".into(),scope:Some(path.clone()) }),Some(path));
    }
    #[test]
    fn working_directory_and_arguments_remain_separate_process_parameters() {
        let directory=Path::new(r"E:\中文 空格 & 资料");
        let command=shell_command(Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe"), &["-NoProfile"], directory);
        assert_eq!(command.get_current_dir(),Some(directory));
        assert_eq!(command.get_args().collect::<Vec<_>>(),[OsStr::new("-NoProfile")]);
    }
}
