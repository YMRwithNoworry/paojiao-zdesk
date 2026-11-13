//! ZSDesk Windows 平台层：集中封装 `unsafe` 与 Windows API。
//!
//! 业务 crate 禁止 `unsafe`，这里不设限制，但所有接口必须收敛为安全 API。

pub mod enumerate;
pub mod flyout_layout;

#[cfg(windows)]
pub mod desktop;
#[cfg(windows)]
pub mod gfx;
#[cfg(windows)]
pub mod integration;
#[cfg(windows)]
pub mod flyout;
#[cfg(windows)]
pub mod ole;
#[cfg(windows)]
pub mod shell;
#[cfg(windows)]
pub mod taskbar;
#[cfg(windows)]
pub mod theme;
