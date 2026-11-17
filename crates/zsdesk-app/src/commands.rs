/// Keyboard and chrome commands handled by the Explorer window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    NewTab,
    CloseTab,
    CloseOtherTabs,
    NextTab,
    PreviousTab,
    Back,
    Forward,
    Up,
    FocusAddress,
    Refresh,
    AddBookmark,
    ReopenTab,
    FocusSearch,
    NewFolder,
    CopyPath,
    TogglePreview,
    ToggleDetails,
    ToggleDualPane,
    SwitchPane,
    NewWindow,
    ToggleFullscreen,
    ToggleCommandBar,
    FocusNext,
    FocusPrevious,
    ZoomIn,
    ZoomOut,
    ResetZoom,
    ViewPreset(u8),
    /// Ctrl+1 … Ctrl+9 (9 selects the last tab, like browsers).
    SelectTab(u8),
}

impl Command {
    pub const fn pane_specific(self)->bool {
        matches!(self,Self::NewTab|Self::CloseTab|Self::CloseOtherTabs|Self::NextTab|Self::PreviousTab|Self::Back|Self::Forward|Self::Up|
            Self::Refresh|Self::ReopenTab|Self::NewFolder|Self::CopyPath|Self::ZoomIn|Self::ZoomOut|Self::ResetZoom|
            Self::ViewPreset(_)|Self::SelectTab(_))
    }
    pub const fn id(self) -> u16 {
        match self {
            Self::NewTab => 2001,
            Self::CloseTab => 2002,
            Self::NextTab => 2003,
            Self::PreviousTab => 2004,
            Self::Back => 2005,
            Self::Forward => 2006,
            Self::Up => 2007,
            Self::FocusAddress => 2008,
            Self::Refresh => 2009,
            Self::AddBookmark => 2010,
            Self::ReopenTab => 2011,
            Self::FocusSearch => 2012,
            Self::NewFolder => 2013,
            Self::CopyPath => 2014,
            Self::TogglePreview => 2015,
            Self::ToggleDetails => 2016,
            Self::ToggleDualPane => 2017,
            Self::SwitchPane => 2018,
            Self::NewWindow => 2030,
            Self::ToggleFullscreen => 2031,
            Self::FocusNext => 2032,
            Self::FocusPrevious => 2033,
            Self::ZoomIn => 2034,
            Self::ZoomOut => 2035,
            Self::ResetZoom => 2036,
            Self::CloseOtherTabs => 2037,
            Self::ToggleCommandBar => 2038,
            Self::ViewPreset(n) => 2040 + n as u16,
            Self::SelectTab(n) => 2020 + n as u16,
        }
    }

    pub const fn from_id(id: u16) -> Option<Self> {
        match id {
            2001 => Some(Self::NewTab),
            2002 => Some(Self::CloseTab),
            2003 => Some(Self::NextTab),
            2004 => Some(Self::PreviousTab),
            2005 => Some(Self::Back),
            2006 => Some(Self::Forward),
            2007 => Some(Self::Up),
            2008 => Some(Self::FocusAddress),
            2009 => Some(Self::Refresh),
            2010 => Some(Self::AddBookmark),
            2011 => Some(Self::ReopenTab),
            2012 => Some(Self::FocusSearch),
            2013 => Some(Self::NewFolder),
            2014 => Some(Self::CopyPath),
            2015 => Some(Self::TogglePreview),
            2016 => Some(Self::ToggleDetails),
            2017 => Some(Self::ToggleDualPane),
            2018 => Some(Self::SwitchPane),
            2030 => Some(Self::NewWindow),
            2031 => Some(Self::ToggleFullscreen),
            2032 => Some(Self::FocusNext),
            2033 => Some(Self::FocusPrevious),
            2034 => Some(Self::ZoomIn),
            2035 => Some(Self::ZoomOut),
            2036 => Some(Self::ResetZoom),
            2037 => Some(Self::CloseOtherTabs),
            2038 => Some(Self::ToggleCommandBar),
            2041..=2048 => Some(Self::ViewPreset((id - 2040) as u8)),
            2021..=2029 => Some(Self::SelectTab((id - 2020) as u8)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Command;
    #[test]
    fn ids_round_trip() {
        for c in [Command::NewTab, Command::CloseOtherTabs, Command::NewFolder, Command::ToggleDetails,Command::ToggleDualPane,Command::SwitchPane,Command::ToggleCommandBar, Command::SelectTab(1), Command::SelectTab(9)] {
            assert_eq!(Command::from_id(c.id()), Some(c));
        }
        assert_eq!(Command::from_id(2020), None);
    }
}
