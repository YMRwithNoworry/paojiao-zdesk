//! Bounded per-tab snapshots; inactive tabs retain no Shell view or HWND.
use windows::Win32::{Foundation::PROPERTYKEY, System::Com::CoTaskMemFree, UI::{HiDpi::GetDpiForWindow, Shell::{
    Common::ITEMIDLIST, IEnumIDList, IFolderView2, IColumnManager, IShellView, ILGetSize, FOLDERVIEWMODE, SVGIO_SELECTION,
    SVSI_SELECT, SVSI_DESELECTOTHERS, SVSI_FOCUSED, SVSI_ENSUREVISIBLE,
    SORTCOLUMN, SORT_ASCENDING, SORT_DESCENDING, CM_COLUMNINFO, CM_ENUM_VISIBLE, CM_MASK_WIDTH,
}}};
use windows::core::{BOOL, GUID, HRESULT, Interface};
use zsdesk_core::model::explorer::{ShellViewState, ViewPropertyKey, ViewSortColumn, ViewColumn, ViewGrouping};
use super::ExplorerBrowserHost;

/// Folder templates may restore their own flags after a view is created.
/// Apply only the header policy after navigation or an explicit mode change;
/// never from OnStateChange, where SetCurrentFolderFlags can re-enter itself.
pub(super) fn sync_column_header(view:&IFolderView2)->windows::core::Result<()>{
    use windows::Win32::UI::Shell::{FVM_DETAILS,FWF_NOHEADERINALLVIEWS,FWF_NOCOLUMNHEADER};
    let mut mode=FOLDERVIEWMODE(0);let mut icon=0;
    unsafe{view.GetViewModeAndIconSize(&mut mode,&mut icon)}?;
    let mask=(FWF_NOHEADERINALLVIEWS|FWF_NOCOLUMNHEADER).0 as u32;
    let desired=(FWF_NOHEADERINALLVIEWS.0|if mode==FVM_DETAILS{0}else{FWF_NOCOLUMNHEADER.0})as u32;
    if unsafe{view.GetCurrentFolderFlags()}?&mask!=desired{unsafe{view.SetCurrentFolderFlags(mask,desired)}?;}
    Ok(())
}

/// View-only settings for an isolated background menu; no selection or paths.
pub fn capture_view_layout(shell:&IShellView)->Option<ShellViewState> {
    let view:IFolderView2=shell.cast().ok()?;
    let mut mode=FOLDERVIEWMODE(0);let mut icon_size=0;
    unsafe{view.GetViewModeAndIconSize(&mut mode,&mut icon_size).ok()?;}
    Some(ShellViewState{mode:mode.0,icon_size,
        sort_columns:read_sort_columns(&view).unwrap_or_default().into_iter().map(|column|ViewSortColumn{key:encode_key(column.propkey),ascending:column.direction==SORT_ASCENDING}).collect(),
        grouping:read_grouping(&view).map(|(key,ascending)|ViewGrouping{key:(key!=NULL_KEY).then(||encode_key(key)),ascending}),
        ..Default::default()})
}
/// Restores menu-controlled layout without clearing selection or moving focus.
pub fn restore_view_layout(shell:&IShellView,state:&ShellViewState) {
    let Ok(view)=shell.cast::<IFolderView2>()else{return;};
    if (1..=8).contains(&state.mode)&&(8..=512).contains(&state.icon_size) {
        let _=unsafe{view.SetViewModeAndIconSize(FOLDERVIEWMODE(state.mode),state.icon_size)};
    }
    restore_layout(shell,&view,state);
    let _=sync_column_header(&view);
}

impl ExplorerBrowserHost {
    pub fn select_children(&self,names:&[std::ffi::OsString])->windows::core::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows::{core::PCWSTR,Win32::{Foundation::HWND,UI::Shell::IShellFolder}};
        let shell=unsafe{self.current_view()?};let view:IFolderView2=shell.cast()?;let folder:IShellFolder=unsafe{view.GetFolder()?};
        for(index,name)in names.iter().take(128).enumerate(){
            let name:Vec<u16>=name.encode_wide().chain(Some(0)).collect();let mut pidl=std::ptr::null_mut();
            unsafe{folder.ParseDisplayName(HWND::default(),None,PCWSTR(name.as_ptr()),None,&mut pidl,std::ptr::null_mut())?;}
            let flags=SVSI_SELECT.0|if index==0{SVSI_DESELECTOTHERS.0|SVSI_FOCUSED.0|SVSI_ENSUREVISIBLE.0}else{0};
            let result=(||{unsafe{shell.SelectItem(pidl,flags as u32)}?;
                let selected=unsafe{view.GetSelectionState(pidl)}?;
                if selected&SVSI_SELECT.0 as u32==0{return Err(windows::core::Error::from_hresult(HRESULT(0x8000000au32 as i32)));}
                Ok(())})();
            unsafe{CoTaskMemFree(Some(pidl.cast()))};result?;
        }
        Ok(())
    }
    pub fn select_child(&self, name:&std::ffi::OsStr) -> windows::core::Result<()> {
        self.select_children(&[name.to_os_string()])
    }
    /// Start editing the exact newly created item only after it is materialized.
    pub fn rename_child(&self, name:&std::ffi::OsStr) -> windows::core::Result<()> {
        self.select_child(name)?;
        use std::os::windows::ffi::OsStrExt;
        use windows::{core::PCWSTR,Win32::{Foundation::HWND,UI::Shell::IShellFolder}};
        let shell=unsafe {self.current_view()?};
        let view:IFolderView2=shell.cast()?;
        let folder:IShellFolder=unsafe {view.GetFolder()?};
        let name:Vec<u16>=name.encode_wide().chain(Some(0)).collect();
        let mut pidl=std::ptr::null_mut();
        unsafe {folder.ParseDisplayName(HWND::default(),None,PCWSTR(name.as_ptr()),None,&mut pidl,std::ptr::null_mut())?;}
        let result=unsafe {shell.SelectItem(pidl,(SVSI_SELECT.0|SVSI_DESELECTOTHERS.0|SVSI_FOCUSED.0|SVSI_ENSUREVISIBLE.0|windows::Win32::UI::Shell::SVSI_EDIT.0) as u32)};
        unsafe {CoTaskMemFree(Some(pidl.cast()))};
        result
    }
    pub fn view_mode(&self) -> Option<(i32,i32)> {
        let view: IFolderView2 = unsafe { self.current_view().ok()?.cast().ok()? };
        let mut mode=FOLDERVIEWMODE(0);let mut icon=0;
        unsafe { view.GetViewModeAndIconSize(&mut mode,&mut icon).ok()?; }
        Some((mode.0,icon))
    }
    pub fn set_view_mode(&self, mode:i32, icon:i32) -> windows::core::Result<()> {
        let view: IFolderView2 = unsafe { self.current_view()?.cast()? };
        unsafe { view.SetViewModeAndIconSize(FOLDERVIEWMODE(mode),icon) }?;
        sync_column_header(&view)
    }
    pub unsafe fn capture_view(&self) -> Option<ShellViewState> {
        let shell=unsafe {self.current_view().ok()?};
        let view: IFolderView2 = shell.cast().ok()?;
        let mut mode=FOLDERVIEWMODE(0); let mut icon_size=0;
        unsafe { view.GetViewModeAndIconSize(&mut mode,&mut icon_size).ok()?; }
        let mut selected=Vec::new(); let mut budget=0;
        // Some native DefView builds return the first selected index regardless
        // of GetSelectedItem's start argument. Enumerate the selected child PIDLs
        // directly, preserving their identity within virtual folders as well.
        if let Ok(items)=unsafe {view.Items::<IEnumIDList>(SVGIO_SELECTION)} {
            for _ in 0..256 {
                let mut pointers=[std::ptr::null_mut()];let mut fetched=0;
                let status=unsafe {items.Next(&mut pointers,Some(&mut fetched))};
                // Failed enumerations do not transfer valid output entries.
                if status.is_err() || fetched!=1 {break;}
                if let Some(bytes)=unsafe {take_item_bytes(pointers[0])} {
                    budget += bytes.len(); if budget > 128*1024 {break;}
                    selected.push(bytes);
                }
                if status!=HRESULT(0) {break;}
            }
        }
        let mut focused_index=-1;
        let focused_status=unsafe {(view.vtable().base__.GetFocusedItem)(view.as_raw(),&mut focused_index)};
        let focused=if focused_status==HRESULT(0) && focused_index>=0 {unsafe {item_bytes(&view,focused_index)}}else{None};
        let mut visible_index=-1;
        let visible_status=unsafe {(view.vtable().GetVisibleItem)(view.as_raw(),-1,false.into(),&mut visible_index)};
        let first_visible=if visible_status==HRESULT(0) && visible_index>=0 {unsafe {item_bytes(&view,visible_index)}}else{None};
        let sort_columns=read_sort_columns(&view).unwrap_or_default().into_iter()
            .map(|column|ViewSortColumn{key:encode_key(column.propkey),ascending:column.direction==SORT_ASCENDING}).collect();
        let columns=shell.cast::<IColumnManager>().ok().and_then(|manager|read_columns(&manager)).unwrap_or_default();
        let grouping=read_grouping(&view).map(|(key,ascending)|ViewGrouping{
            key:(key!=NULL_KEY).then(||encode_key(key)),ascending});
        let column_dpi=if columns.is_empty(){0}else{view_dpi(&shell)};
        Some(ShellViewState {mode:mode.0,icon_size,selected,focused,first_visible,sort_columns,columns,grouping,column_dpi})
    }
    pub unsafe fn restore_view(&self,state:&ShellViewState) {
        let Ok(shell_view)=(unsafe {self.current_view()}) else {return;};
        let Ok(view)=shell_view.cast::<IFolderView2>() else {return;};
        if (1..=8).contains(&state.mode) && (8..=512).contains(&state.icon_size) {
            let mut mode=FOLDERVIEWMODE(0);let mut icon=0;
            let unchanged=unsafe {view.GetViewModeAndIconSize(&mut mode,&mut icon)}.is_ok()
                && (mode.0,icon)==(state.mode,state.icon_size);
            if !unchanged {let _=unsafe {view.SetViewModeAndIconSize(FOLDERVIEWMODE(state.mode),state.icon_size)};}
        }
        restore_layout(&shell_view,&view,state);
        let _=sync_column_header(&view);
        // Clear once. Passing DESELECTOTHERS to SelectAndPositionItems applies
        // it to every item and leaves only the last item selected on DefView.
        let _=unsafe {shell_view.SelectItem(std::ptr::null(),SVSI_DESELECTOTHERS.0 as u32)};
        // The SDK's SVSI_NOSTATECHANGE macro is outside the _SVSIF enum.
        const NO_STATE_CHANGE:u32=0x8000_0000;
        if let Some(pidl)=state.first_visible.as_ref().filter(|p|valid_pidl(p)) {
            let _=unsafe {shell_view.SelectItem(pidl.as_ptr().cast(),SVSI_ENSUREVISIBLE.0 as u32|NO_STATE_CHANGE)};
        }
        let mut selected:Vec<_>=state.selected.iter().take(256).filter(|p|valid_pidl(p)).collect();
        let mut flags=SVSI_SELECT.0 as u32;
        if let Some(pidl)=state.focused.as_ref().filter(|p|valid_pidl(p)) {
            if let Some(index)=selected.iter().position(|selected|*selected==pidl) {
                // DefView focuses the final item in a batch. Keep this in the
                // same submission so a later single-item request cannot replace
                // the pending selection while a newly created view enumerates.
                let focused=selected.remove(index);selected.push(focused);
                flags|=SVSI_FOCUSED.0 as u32;
            } else {
                let _=unsafe {shell_view.SelectItem(pidl.as_ptr().cast(),SVSI_FOCUSED.0 as u32)};
            }
        }
        if !selected.is_empty() {
            let pidls:Vec<*const ITEMIDLIST>=selected.iter().map(|p|p.as_ptr().cast()).collect();
            let _=unsafe {view.SelectAndPositionItems(pidls.len() as u32,pidls.as_ptr(),None,flags)};
        }
    }
}

const MAX_COLUMNS:usize=64;
const MAX_COLUMN_WIDTH:u32=16_384;
const NULL_KEY:PROPERTYKEY=PROPERTYKEY{fmtid:GUID::zeroed(),pid:0};

fn encode_key(key:PROPERTYKEY)->ViewPropertyKey {
    ViewPropertyKey{format_id:format!("{:?}",key.fmtid),property_id:key.pid}
}
fn decode_key(key:&ViewPropertyKey)->Option<PROPERTYKEY> {
    let text=match key.format_id.len() {
        36=>key.format_id.as_str(),
        38=>key.format_id.strip_prefix('{')?.strip_suffix('}')?,
        _=>return None,
    };
    let fmtid=GUID::try_from(text).ok()?;
    (fmtid!=GUID::zeroed()).then_some(PROPERTYKEY{fmtid,pid:key.property_id})
}
fn view_dpi(shell:&IShellView)->u32 {
    unsafe {shell.GetWindow()}.ok().map(|window|unsafe {GetDpiForWindow(window)}).filter(|dpi|*dpi>0).unwrap_or(96)
}
fn scaled_column_width(width:u32,source_dpi:u32,target_dpi:u32)->Option<u32> {
    if !(1..=MAX_COLUMN_WIDTH).contains(&width) || !(48..=960).contains(&target_dpi) {return None;}
    // Legacy snapshots have no DPI tag. Do not scale an unknown pixel basis.
    let source=if source_dpi==0 {target_dpi}else{source_dpi};
    if !(48..=960).contains(&source) {return None;}
    let value=(u64::from(width)*u64::from(target_dpi)+u64::from(source)/2)/u64::from(source);
    (1..=u64::from(MAX_COLUMN_WIDTH)).contains(&value).then_some(value as u32)
}
fn visible_column_keys(manager:&IColumnManager)->Option<Vec<PROPERTYKEY>> {
    let count=unsafe {manager.GetColumnCount(CM_ENUM_VISIBLE)}.ok()? as usize;
    if count==0 || count>MAX_COLUMNS {return None;}
    let mut keys=vec![PROPERTYKEY::default();count];
    unsafe {manager.GetColumns(CM_ENUM_VISIBLE,&mut keys)}.ok()?;
    if keys.iter().any(|key|key.fmtid==GUID::zeroed()) {return None;}
    Some(keys)
}
fn column_info(manager:&IColumnManager,key:&PROPERTYKEY)->Option<CM_COLUMNINFO> {
    let mut info=CM_COLUMNINFO{cbSize:std::mem::size_of::<CM_COLUMNINFO>() as u32,dwMask:CM_MASK_WIDTH.0 as u32,..Default::default()};
    unsafe {manager.GetColumnInfo(key,&mut info)}.ok()?;
    Some(info)
}
fn read_columns(manager:&IColumnManager)->Option<Vec<ViewColumn>> {
    let keys=visible_column_keys(manager)?;
    keys.into_iter().map(|key|{
        let width=column_info(manager,&key)?.uWidth;
        (1..=MAX_COLUMN_WIDTH).contains(&width).then(||ViewColumn{key:encode_key(key),width})
    }).collect()
}
fn read_sort_columns(view:&IFolderView2)->Option<Vec<SORTCOLUMN>> {
    let count=unsafe {view.GetSortColumnCount()}.ok()?;
    if !(0..=MAX_COLUMNS as i32).contains(&count) {return None;}
    let mut columns=vec![SORTCOLUMN::default();count as usize];
    if count>0 {unsafe {view.GetSortColumns(&mut columns)}.ok()?;}
    if columns.iter().any(|column|column.propkey.fmtid==GUID::zeroed()
        || ![SORT_ASCENDING,SORT_DESCENDING].contains(&column.direction)) {return None;}
    Some(columns)
}
fn read_grouping(view:&IFolderView2)->Option<(PROPERTYKEY,bool)> {
    let mut key=NULL_KEY;let mut ascending=BOOL(1);
    let status=unsafe {(view.vtable().GetGroupBy)(view.as_raw(),&mut key,&mut ascending)};
    if status==HRESULT(1) {Some((NULL_KEY,true))}
    else if status==HRESULT(0) {Some((key,if key==NULL_KEY {true}else{ascending.as_bool()}))}
    else {None}
}
fn restore_layout(shell:&IShellView,view:&IFolderView2,state:&ShellViewState) {
    if !state.columns.is_empty() && state.columns.len()<=MAX_COLUMNS {
        let target_dpi=view_dpi(shell);
        let desired:Option<Vec<_>>=state.columns.iter().map(|column|
            Some((decode_key(&column.key)?,scaled_column_width(column.width,state.column_dpi,target_dpi)?))).collect();
        if let (Some(desired),Ok(manager))=(desired,shell.cast::<IColumnManager>()) {
            let keys:Vec<_>=desired.iter().map(|(key,_)|*key).collect();
            let distinct=keys.iter().enumerate().all(|(index,key)|!keys[..index].contains(key));
            // Unknown property/namespace state is left to the Shell defaults.
            if distinct && keys.iter().all(|key|column_info(&manager,key).is_some()) {
                if let Some(current)=visible_column_keys(&manager) {
                    let order_ready=current==keys || unsafe {manager.SetColumns(&keys)}.is_ok();
                    if order_ready {
                        for (key,width) in desired {
                            if column_info(&manager,&key).is_some_and(|info|info.uWidth!=width) {
                                let info=CM_COLUMNINFO{cbSize:std::mem::size_of::<CM_COLUMNINFO>() as u32,
                                    dwMask:CM_MASK_WIDTH.0 as u32,uWidth:width,..Default::default()};
                                let _=unsafe {manager.SetColumnInfo(&key,&info)};
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(group)=&state.grouping {
        let desired=match &group.key {Some(key)=>decode_key(key).map(|key|(key,group.ascending)),None=>Some((NULL_KEY,true))};
        if let (Some((key,ascending)),Some(current))=(desired,read_grouping(view)) {
            if current!=(key,ascending) {let _=unsafe {view.SetGroupBy(&key,ascending)};}
        }
    }
    // Grouping can change the effective sort order; apply the captured sort last.
    if !state.sort_columns.is_empty() && state.sort_columns.len()<=MAX_COLUMNS {
        let desired:Option<Vec<_>>=state.sort_columns.iter().map(|column|Some(SORTCOLUMN{
            propkey:decode_key(&column.key)?,direction:if column.ascending {SORT_ASCENDING}else{SORT_DESCENDING}})).collect();
        if let (Some(desired),Some(current))=(desired,read_sort_columns(view)) {
            if current!=desired {let _=unsafe {view.SetSortColumns(&desired)};}
        }
    }
}

unsafe fn item_bytes(view:&IFolderView2,index:i32)->Option<Vec<u8>> {
    let pointer: *mut ITEMIDLIST=unsafe {view.Item(index).ok()?};
    unsafe {take_item_bytes(pointer)}
}
unsafe fn take_item_bytes(pointer:*mut ITEMIDLIST)->Option<Vec<u8>> {
    if pointer.is_null() {return None;}
    let len=unsafe {ILGetSize(Some(pointer))} as usize;
    let result=if (2..=8192).contains(&len) && !pointer.is_null() {
        Some(unsafe {std::slice::from_raw_parts(pointer.cast::<u8>(),len)}.to_vec())
    } else {None};
    unsafe {CoTaskMemFree(Some(pointer.cast()))}; result
}
fn valid_pidl(bytes:&[u8])->bool {
    if bytes.len()>8192 {return false;}
    let mut at=0;
    while at+2<=bytes.len() {
        let length=u16::from_le_bytes([bytes[at],bytes[at+1]]) as usize;
        if length==0 {return at+2==bytes.len();}
        if length<2 || at+length>bytes.len() {return false;}
        at+=length;
    }
    false
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn rejects_truncated_or_unterminated_snapshot() {
        assert!(valid_pidl(&[4,0,1,2,0,0]));
        assert!(!valid_pidl(&[4,0,1,2])); assert!(!valid_pidl(&[1,0,0,0]));
        assert!(!valid_pidl(&[0,0,1])); assert!(!valid_pidl(&[]));
    }
    #[test] fn column_keys_are_validated_without_panicking() {
        let key=super::super::commands::PKEY_NAME;
        assert_eq!(decode_key(&encode_key(key)),Some(key));
        assert!(decode_key(&ViewPropertyKey{format_id:"not a guid".into(),property_id:10}).is_none());
        assert!(decode_key(&ViewPropertyKey{format_id:"00000000-0000-0000-0000-000000000000".into(),property_id:10}).is_none());
    }
    #[test] fn column_widths_scale_once_and_reject_sentinels() {
        assert_eq!(scaled_column_width(200,96,144),Some(300));
        assert_eq!(scaled_column_width(300,144,96),Some(200));
        assert_eq!(scaled_column_width(200,0,192),Some(200));
        for width in [0,u32::MAX,u32::MAX-1,MAX_COLUMN_WIDTH+1] {assert_eq!(scaled_column_width(width,96,144),None);}
        assert_eq!(scaled_column_width(200,1,144),None);
    }
}
