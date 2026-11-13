//! Shell snapshots whose COM work runs after releasing the window-state borrow.
use std::path::PathBuf;
use windows::{core::Interface, Win32::{Foundation::HWND, System::Com::CoTaskMemFree,
    UI::Shell::{IExplorerBrowser, IFolderView2, IShellView, SIGDN_FILESYSPATH}}};
use zsdesk_core::model::explorer::{PreviewProviderPreference, ShellLocation};
use super::super::Slot;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ViewIdentity {
    owner: isize,
    browser: usize,
    source: usize,
    view: isize,
    tab: u64,
    navigation: u64,
    location: ShellLocation,
    selection: u64,
}

pub(crate) struct ViewSnapshot {
    browser: IExplorerBrowser,
    pub identity: ViewIdentity,
    pub provider: PreviewProviderPreference,
    pub dark: bool,
}

impl ViewSnapshot {
    pub fn capture(slot: &Slot) -> Option<Self> {
        let slot = slot.try_borrow().ok()?;
        let state = slot.as_ref()?;
        if state.pending.is_some() || state.queued.is_some() || state.view_hwnd.0.is_null()
            || state.shown_tab != Some(state.session.active_tab().id) { return None; }
        let host = state.browser.as_ref()?;
        // browser_handle only AddRefs the stored interface; it never calls Shell.
        let browser = host.browser_handle();
        let identity = ViewIdentity { owner: state.hwnd.0 as isize,
            browser: browser.as_raw() as usize, source:state.browser_source,view:state.view_hwnd.0 as isize,tab: state.session.active_tab().id,
            navigation: state.navigation_epoch, location: state.current_location().clone(),
            selection: host.search_selection_revision() };
        Some(Self { browser, identity, provider: state.session.ui.preview_provider, dark: state.dark })
    }

    pub fn tab(&self) -> u64 { self.identity.tab }
    pub fn source(&self) -> usize { self.identity.source }
    pub fn input_identity(&self)->super::super::pane_input::Identity{
        super::super::pane_input::Identity{source:self.identity.source,tab:self.identity.tab,epoch:self.identity.navigation}
    }
    pub fn owner(&self) -> HWND { HWND(self.identity.owner as *mut _) }
    pub fn window(&self) -> HWND { HWND(self.identity.view as *mut _) }

    pub fn current(&self, slot: &Slot, selection: bool) -> bool {
        Self::capture(slot).is_some_and(|current| {
            self.identity.same_navigation(&current.identity)
                && (!selection || self.identity.selection == current.identity.selection)
        })
    }

    pub fn view(&self, slot: &Slot) -> Option<IShellView> {
        if !self.current(slot, false) { return None; }
        let view = unsafe { self.browser.GetCurrentView::<IShellView>() }.ok()?;
        self.current(slot, false).then_some(view)
    }

    pub fn selected_path(&self, slot: &Slot) -> Option<PathBuf> {
        let view: IFolderView2 = self.view(slot)?.cast().ok()?;
        let selected = unsafe { view.GetSelection(false) }.ok()?;
        let item = unsafe { selected.GetItemAt(0) }.ok()?;
        let text = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.ok()?;
        let path = unsafe { text.to_string() }.ok().map(PathBuf::from);
        unsafe { CoTaskMemFree(Some(text.0.cast())) };
        path.filter(|path| path.is_absolute() && self.current(slot, true))
    }
}

impl ViewIdentity {
    fn same_navigation(&self, other: &Self) -> bool {
        self.owner == other.owner && self.browser == other.browser && self.source==other.source && self.view==other.view && self.tab == other.tab
            && self.navigation == other.navigation && self.location == other.location
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returning_to_same_folder_does_not_revalidate_a_stale_snapshot() {
        let original = ViewIdentity { owner: 1, browser: 2, source:7,view:3,tab: 17, navigation: 4,
            location: ShellLocation::Path(PathBuf::from(r"C:\files")), selection: 3 };
        let mut changed = original.clone();
        changed.navigation += 2;
        assert!(!original.same_navigation(&changed));
        changed = original.clone(); changed.tab += 1;
        assert!(!original.same_navigation(&changed));
        changed = original.clone(); changed.browser += 1;
        assert!(!original.same_navigation(&changed));
        changed = original.clone(); changed.selection += 1;
        assert!(original.same_navigation(&changed));
        assert_ne!(original, changed);
    }
}
