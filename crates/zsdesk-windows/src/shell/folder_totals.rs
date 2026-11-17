//! A view-local total-size column; filesystem properties remain unchanged.
use std::{cell::RefCell,collections::HashMap,sync::atomic::{AtomicUsize,Ordering}};
use windows::{core::{w,GUID,HRESULT,Interface,Result},Win32::{Foundation::{HANDLE,HWND,PROPERTYKEY},
    UI::{Shell::{IShellView,IFolderView2,IColumnManager,CM_COLUMNINFO,CM_ENUM_VISIBLE,CM_MASK_WIDTH,SORTCOLUMN},
        WindowsAndMessaging::{GetPropW,SetPropW,RemovePropW}}}};
use super::commands::PKEY_SIZE;

pub const PKEY_TOTAL_SIZE:PROPERTYKEY=PROPERTYKEY{fmtid:GUID::from_u128(0x28636AA6_953D_11D2_B5D6_00C04FD918D0),pid:14};
const PROPERTY:windows::core::PCWSTR=w!("ZSDesk.TotalSizeColumn");
static NEXT:AtomicUsize=AtomicUsize::new(2);
const PKEY_PARENT:PROPERTYKEY=PROPERTYKEY{fmtid:GUID::from_u128(0xE3E0584C_B788_4A5A_BB20_7F5A44C9ACDD),pid:6};
#[derive(Clone)]struct Order{base:Vec<SORTCOLUMN>,applied:Vec<SORTCOLUMN>,previous:Vec<SORTCOLUMN>,extra:bool}
thread_local!{static ORDERS:RefCell<HashMap<(isize,usize),Order>>=RefCell::new(HashMap::new());}
fn token(view:&IShellView)->usize{unsafe{view.GetWindow()}.ok().map_or(0,|window|unsafe{GetPropW(window,PROPERTY)}.0 as usize)}
fn identity(view:&IShellView)->Option<(isize,usize)>{let token=token(view)&!1;if token==0{return None;}Some((unsafe{view.GetWindow()}.ok()?.0 as isize,token))}
pub(super) fn base_sort_columns(view:&IShellView,columns:Vec<SORTCOLUMN>)->Vec<SORTCOLUMN>{
    identity(view).and_then(|key|ORDERS.with(|orders|orders.borrow().get(&key).filter(|state|state.applied==columns||state.previous==columns).map(|state|state.base.clone()))).unwrap_or(columns)
}
pub fn enabled(view:&IShellView)->bool{token(view)!=0}
pub fn ensure_size_visible(view:&IShellView)->Result<()>{let manager:IColumnManager=view.cast()?;let mut visible=keys(&manager)?;
    if !visible.contains(&PKEY_SIZE)&&!visible.contains(&PKEY_TOTAL_SIZE){visible.push(PKEY_SIZE);unsafe{manager.SetColumns(&visible)}?;}Ok(())}
pub fn logical_sort_key(view:&IShellView,key:PROPERTYKEY)->PROPERTYKEY{if enabled(view)&&key==PKEY_TOTAL_SIZE{PKEY_SIZE}else{key}}
pub fn sort_key(view:&IShellView,key:PROPERTYKEY)->PROPERTYKEY{if enabled(view)&&key==PKEY_SIZE{PKEY_TOTAL_SIZE}else{key}}
pub fn logical_column_key(view:&IShellView,key:PROPERTYKEY)->PROPERTYKEY{if token(view)&1!=0&&key==PKEY_TOTAL_SIZE{PKEY_SIZE}else{key}}
pub fn column_key(view:&IShellView,key:PROPERTYKEY)->PROPERTYKEY{if token(view)&1!=0&&key==PKEY_SIZE{PKEY_TOTAL_SIZE}else{key}}
fn keys(columns:&IColumnManager)->Result<Vec<PROPERTYKEY>>{
    let count=unsafe{columns.GetColumnCount(CM_ENUM_VISIBLE)}?;
    if count>128{return Err(windows::core::Error::from_hresult(HRESULT(0x80070057u32 as i32)));}
    let mut keys=vec![PKEY_SIZE;count as usize];if count>0{unsafe{columns.GetColumns(CM_ENUM_VISIBLE,&mut keys)}?;}Ok(keys)
}
fn width(columns:&IColumnManager,key:&PROPERTYKEY)->Option<u32>{
    let mut info=CM_COLUMNINFO{cbSize:std::mem::size_of::<CM_COLUMNINFO>()as u32,dwMask:CM_MASK_WIDTH.0 as u32,..Default::default()};
    unsafe{columns.GetColumnInfo(key,&mut info)}.ok()?;(info.uWidth>0&&info.uWidth<=32768).then_some(info.uWidth)
}
fn set_width(columns:&IColumnManager,key:&PROPERTYKEY,value:Option<u32>){if let Some(value)=value{
    let info=CM_COLUMNINFO{cbSize:std::mem::size_of::<CM_COLUMNINFO>()as u32,dwMask:CM_MASK_WIDTH.0 as u32,uWidth:value,..Default::default()};
    let _=unsafe{columns.SetColumnInfo(key,&info)};
}}
fn remap_sort(shell:&IShellView,from:PROPERTYKEY,to:PROPERTYKEY)->Result<()>{
    let view:IFolderView2=shell.cast()?;
    let count=unsafe{view.GetSortColumnCount()}?;if !(0..=128).contains(&count){return Err(windows::core::Error::from_hresult(HRESULT(0x80070057u32 as i32)));}
    let mut columns=vec![SORTCOLUMN::default();count as usize];if count>0{unsafe{view.GetSortColumns(&mut columns)}?;}
    let original=columns.clone();let mut columns=base_sort_columns(shell,columns);
    let mut changed=columns!=original;for column in &mut columns{if column.propkey==from{column.propkey=to;changed=true;}}
    let mut group=PROPERTYKEY::default();let mut ascending=windows::core::BOOL(1);
    if unsafe{view.GetGroupBy(&mut group,Some(&mut ascending))}.is_ok()&&group==from{unsafe{view.SetGroupBy(&to,ascending.as_bool())}?;}
    if changed{unsafe{view.SetSortColumns(&columns)}?;}
    Ok(())
}

/// Only the controller holding this token can undo the column transformation.
pub struct Columns{view:IShellView,window:HWND,token:usize,introduced:bool,active:bool}
impl Columns{
    pub fn activate(view:&IShellView)->Result<Self>{
        let window=unsafe{view.GetWindow()}?;
        if token(view)!=0{return Err(windows::core::Error::from_hresult(HRESULT(0x8000FFFFu32 as i32)));}
        let value=NEXT.fetch_add(2,Ordering::Relaxed).max(2)&!1;
        unsafe{SetPropW(window,PROPERTY,Some(HANDLE(value as*mut _)))}?;
        let mut lease=Self{view:view.clone(),window,token:value,introduced:false,active:true};
        lease.synchronize()?;Ok(lease)
    }
    fn owns(&self)->bool{self.active&&unsafe{self.view.GetWindow()}.is_ok_and(|window|window==self.window)
        &&unsafe{GetPropW(self.window,PROPERTY)}.0 as usize==self.token}
    pub fn synchronize(&mut self)->Result<()>{
        if !self.owns(){return Err(windows::core::Error::from_hresult(HRESULT(0x80070490u32 as i32)));}
        let columns:IColumnManager=self.view.cast()?;let mut visible=keys(&columns)?;
        if visible.contains(&PKEY_SIZE)&&!visible.contains(&PKEY_TOTAL_SIZE){
            let previous_width=width(&columns,&PKEY_SIZE);
            for key in &mut visible{if *key==PKEY_SIZE{*key=PKEY_TOTAL_SIZE;}}
            unsafe{columns.SetColumns(&visible)}?;self.introduced=true;self.token|=1;
            unsafe{SetPropW(self.window,PROPERTY,Some(HANDLE(self.token as*mut _)))}?;
            set_width(&columns,&PKEY_TOTAL_SIZE,previous_width);
        }
        // Managed refresh keys are removed only when changing the size mode.
        let current:IFolderView2=self.view.cast()?;let count=unsafe{current.GetSortColumnCount()}?;
        let mut sorts=vec![SORTCOLUMN::default();count.max(0).min(128)as usize];if !sorts.is_empty(){unsafe{current.GetSortColumns(&mut sorts)}?;}
        let mut group=PROPERTYKEY::default();let _=unsafe{current.GetGroupBy(&mut group,None)};
        if sorts.iter().any(|column|column.propkey==PKEY_SIZE)||group==PKEY_SIZE{remap_sort(&self.view,PKEY_SIZE,PKEY_TOTAL_SIZE)?;}
        Ok(())
    }
    pub fn restore(&mut self)->Result<()>{
        if !self.owns(){self.active=false;return Ok(());}
        let columns:IColumnManager=self.view.cast()?;let visible=keys(&columns)?;
        if self.introduced&&visible.contains(&PKEY_TOTAL_SIZE){
            let had_size=visible.contains(&PKEY_SIZE);let previous_width=width(&columns,&PKEY_TOTAL_SIZE);
            let restored:Vec<_>=visible.into_iter().filter_map(|key|if key==PKEY_TOTAL_SIZE{(!had_size).then_some(PKEY_SIZE)}else{Some(key)}).collect();
            unsafe{columns.SetColumns(&restored)}?;if !had_size{set_width(&columns,&PKEY_SIZE,previous_width);}
        }
        remap_sort(&self.view,PKEY_TOTAL_SIZE,PKEY_SIZE)?;
        let _=unsafe{RemovePropW(self.window,PROPERTY)};self.active=false;Ok(())
    }
}
impl Drop for Columns{fn drop(&mut self){let _=self.restore();ORDERS.with(|orders|{orders.borrow_mut().remove(&(self.window.0 as isize,self.token&!1));});}}

/// Resort in the current direction after a batch changes cached values.
pub fn refresh_order(shell:&IShellView)->Result<()>{
    let view:IFolderView2=shell.cast()?;let count=unsafe{view.GetSortColumnCount()}?;
    if count<=0||count>128{return Ok(());}
    let mut current=vec![SORTCOLUMN::default();count as usize];unsafe{view.GetSortColumns(&mut current)}?;
    if current.iter().any(|column|column.propkey==PKEY_TOTAL_SIZE){
        let Some(key)=identity(shell)else{return Ok(());};
        let previous=ORDERS.with(|orders|orders.borrow().get(&key).cloned());
        let managed=previous.as_ref().filter(|state|state.applied==current||state.previous==current);
        let base=managed.map_or_else(||current.clone(),|state|state.base.clone());let extra=managed.is_none_or(|state|!state.extra);
        let mut desired=base.clone();
        // Every item in an ordinary folder has the same parent path. Toggling
        // this redundant final key refreshes cached values without a temporary
        // different ordering, selection change, or persisted extra sort rule.
        if let Some(column)=desired.iter_mut().find(|column|column.propkey==PKEY_PARENT){if extra{column.direction=if column.direction==windows::Win32::UI::Shell::SORT_ASCENDING{windows::Win32::UI::Shell::SORT_DESCENDING}else{windows::Win32::UI::Shell::SORT_ASCENDING};}}
        else if extra{desired.push(SORTCOLUMN{propkey:PKEY_PARENT,direction:windows::Win32::UI::Shell::SORT_ASCENDING});}
        unsafe{view.SetSortColumns(&desired)}?;
        ORDERS.with(|orders|{orders.borrow_mut().insert(key,Order{base,applied:desired,previous:current,extra});});
    }Ok(())
}
pub fn ordered_by_totals(shell:&IShellView)->bool{
    let Ok(view)=shell.cast::<IFolderView2>()else{return false;};let Ok(count)=(unsafe{view.GetSortColumnCount()})else{return false;};
    if count<=0||count>128{return false;}let mut columns=vec![SORTCOLUMN::default();count as usize];
    unsafe{view.GetSortColumns(&mut columns)}.is_ok()&&columns.iter().any(|column|column.propkey==PKEY_TOTAL_SIZE)
}
