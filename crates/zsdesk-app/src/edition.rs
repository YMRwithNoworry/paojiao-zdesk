//! Build edition, selected with Cargo features (see scripts/package.ps1).
//!
//! | 版本 | features |
//! |---|---|
//! | 标准版 | （无） |
//! | 网络版 | `network`：FTP、SFTP/SSH、SMB 地址与网络驱动器 |
//! | 桌面版 | `desktop`：桌面格子 |
//! | 全量版 | `network` + `desktop`（默认） |

pub const DESKTOP_ONLY:bool=cfg!(feature="desktop-box");
pub const NETWORK: bool = cfg!(feature = "network")&&!DESKTOP_ONLY;
pub const DESKTOP: bool = cfg!(feature = "desktop");
pub const THEMED: bool = cfg!(feature = "themed")&&!DESKTOP_ONLY;

pub const fn profile_name() -> &'static str { if DESKTOP_ONLY{"ZSDesk-DesktopBox"}else if THEMED { "ZSDesk-Theme" } else { "ZSDesk" } }
pub const fn application_id()->&'static str{if DESKTOP_ONLY{"ZSDesk.DesktopBox"}else if THEMED{"ZSDesk.Theme"}else{"ZSDesk.Native"}}
pub const fn desktop_startup_value()->&'static str{if DESKTOP_ONLY{"ZSDesk.DesktopBox.Desktop"}else if THEMED{"ZSDesk.Theme.Desktop"}else{"ZSDesk.Desktop"}}
pub const fn application_mutex() -> &'static str {
    if DESKTOP_ONLY{"Local\\ZSDesk.DesktopBox.Application"}else if THEMED { "Local\\ZSDesk.Theme.Application" } else { "Local\\ZSDesk.Native.Application" }
}

pub const fn key() -> &'static str {
    if DESKTOP_ONLY{return "desktop-box";}
    if THEMED { return "themed"; }
    match (NETWORK, DESKTOP) {
        (true, true) => "full",
        (true, false) => "network",
        (false, true) => "desktop",
        (false, false) => "standard",
    }
}

pub const fn name() -> &'static str {
    if DESKTOP_ONLY{return "桌面盒子独立版";}
    if THEMED { return "主题版"; }
    match (NETWORK, DESKTOP) {
        (true, true) => "全量版",
        (true, false) => "网络版",
        (false, true) => "桌面版",
        (false, false) => "标准版",
    }
}
