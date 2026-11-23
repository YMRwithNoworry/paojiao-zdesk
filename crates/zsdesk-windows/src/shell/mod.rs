//! Native Windows Shell hosts used by the Explorer workspace and desktop fences.

pub mod commands;
pub mod file_transfer;
pub mod file_dialogs;
pub mod folder_totals;
mod events;
mod explorer_browser;
pub mod icons;
pub mod menu;
mod packaged_commands;
mod compact_commands;
mod legacy_commands;
mod packaged_overlay;
mod packaged_state_worker;
pub use packaged_state_worker::maybe_handle_packaged_state_worker;
pub use packaged_state_worker::{packaged_state_worker_test_entry,run_packaged_state_test_worker,pending_packaged_state_workers,packaged_state_cleanup_errors};
pub mod new_menu;
mod namespace_tree;
pub mod notify;
pub mod pidl;
mod search;
mod shell_item;
mod view_state;
pub use view_state::{capture_background_layout,capture_view_layout,restore_view_layout};
pub use search::search_location;

pub use events::{
    acknowledge_selection_changed, WM_BOOKMARK_PATH_DROPPED, WM_SHELL_FOLDER_INVOKED, WM_SHELL_NAVIGATED,
    WM_SHELL_NAVIGATION_FAILED, WM_SHELL_SELECTION_CHANGED, WM_SHELL_PICKER_ACCEPT, WM_TREE_NAVIGATE,
};
pub use explorer_browser::{ExplorerBrowserHost, Panes, MenuDiagnostics, menu_diagnostics};
pub use explorer_browser::{set_folder_open_capture,FolderOpenCapture,FolderOpenCallback,set_open_archives_externally};
pub use namespace_tree::{NamespaceTreeHost,NamespaceTreeOptions};
pub use shell_item::{breadcrumb, child_folders, location_from_shell_item, open_path, shell_item, Crumb};
pub use shell_item::is_recycle_bin_name;
pub(crate) use shell_item::take_pwstr as take;

mod view_drop;
