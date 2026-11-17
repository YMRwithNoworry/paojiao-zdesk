use std::path::Path;
pub(super) fn show(profile:&Path)->anyhow::Result<()>{crate::utility_panel::show_search(profile)}
pub(super) fn is_open()->bool{crate::utility_panel::search_open()}
pub(super) fn close(){crate::utility_panel::close_search();}
