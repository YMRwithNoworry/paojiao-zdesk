//! Fluent rendering of an existing Shell menu. The provider retains ownership
//! of every HMENU, command ID, bitmap, item-data value and command implementation.
use std::{cell::{Cell,RefCell},rc::Rc,time::{Duration,Instant}};
use windows::{core::{w,Error,HRESULT,Interface,PWSTR,Result},Win32::{
    Foundation::{HWND,LPARAM,LRESULT,POINT,RECT,SIZE,WPARAM},
    Graphics::{Direct2D::ID2D1Bitmap,Gdi::*,Imaging::IWICBitmapSource},
    System::LibraryLoader::GetModuleHandleW,
    UI::{Controls::{DRAWITEMSTRUCT,MEASUREITEMSTRUCT,ODT_MENU,ODA_DRAWENTIRE,ODS_SELECTED,ODS_DISABLED,ODS_GRAYED,ODS_CHECKED,ODS_DEFAULT,ODS_NOFOCUSRECT,
        TTTOOLINFOW,TTF_TRACK,TTF_ABSOLUTE,TTM_ADDTOOLW,TTM_TRACKACTIVATE,TTM_TRACKPOSITION,TTM_UPDATETIPTEXTW,TTM_SETMAXTIPWIDTH,TTS_ALWAYSTIP,TTS_NOPREFIX},
        HiDpi::{GetDpiForMonitor,MDT_EFFECTIVE_DPI},
        Input::KeyboardAndMouse::{GetCapture,ReleaseCapture,SetCapture,VIRTUAL_KEY,VK_UP,VK_DOWN,VK_HOME,VK_END,VK_PRIOR,VK_NEXT,VK_LEFT,VK_RIGHT,VK_RETURN,VK_SPACE,VK_ESCAPE,VK_MENU,VK_F10,VK_APPS,VK_LWIN,VK_RWIN,VK_TAB},
        Shell::{IContextMenu,IContextMenu2,IContextMenu3,SetWindowSubclass,RemoveWindowSubclass,DefSubclassProc},WindowsAndMessaging::*}}};
use crate::{flyout_layout::{self as geometry,Anchor,Bounds,LevelLayout,Measured,Metrics,RowKind},
    gfx::{Align,BitmapAlpha,Canvas,Color,Gfx,RectF,Surface,TextStyle}};
use super::ContextMenuOptions;

const CLASS:windows::core::PCWSTR=w!("ZSDesk.ShellFlyout");
const TIMER:usize=0x5a53464c;
const NATIVE_SUBCLASS:usize=0x5a53464e;
const MAX_ITEMS:usize=16384;
const MAX_LEVELS:usize=32;
const HOVER_DELAY:Duration=Duration::from_millis(300);
const AIM_DELAY:Duration=Duration::from_millis(450);
// Independently measured Shell popup metrics. Generic application flyouts keep
// their own layout; these values describe the Shell's command strip and body.
const SHELL_TEXT_LEFT_DIP:f32=42.;
const SHELL_SUBMENU_OVERLAP_DIP:f32=8.;
const SHELL_SEPARATOR_INSET_DIP:f32=6.;
const TOOLBAR_HEIGHT_DIP:f32=60.;
const TOOLBAR_BUTTON_DIP:f32=64.;
const TOOLBAR_PADDING_DIP:f32=2.;
const TOOLBAR_LABEL_DIP:f32=12.;
const TOOLTIP_DELAY:Duration=Duration::from_millis(650);
thread_local!{
    static ACTIVE:Cell<bool>=const{Cell::new(false)};
    static CANCEL:Cell<bool>=const{Cell::new(false)};
    static ROOT:Cell<HWND>=const{Cell::new(HWND(std::ptr::null_mut()))};
    static NATIVE_TRACKING:Cell<bool>=const{Cell::new(false)};
    static SNAPSHOT:RefCell<serde_json::Value>=const{RefCell::new(serde_json::Value::Null)};
    static GRAPHICS:RefCell<Option<Rc<Gfx>>>=const{RefCell::new(None)};
}
pub(super) fn is_open()->bool{ACTIVE.with(Cell::get)}
pub(super) fn popup_window()->Option<HWND>{ROOT.with(|root|{let hwnd=root.get();(!hwnd.0.is_null()&&unsafe{IsWindow(Some(hwnd))}.as_bool()&&unsafe{IsWindowVisible(hwnd)}.as_bool()).then_some(hwnd)})}
pub(super) fn cancel(){if is_open(){CANCEL.with(|flag|flag.set(true));if NATIVE_TRACKING.with(Cell::get){let _=unsafe{EndMenu()};}if let Some(hwnd)=popup_window(){let _=unsafe{PostMessageW(Some(hwnd),WM_NULL,WPARAM(0),LPARAM(0))};}}}
pub(super) fn release_thread_resources(){if is_open(){cancel();return;}GRAPHICS.with(|slot|{slot.borrow_mut().take();});SNAPSHOT.with(|slot|*slot.borrow_mut()=serde_json::Value::Null);}
/// Read-only geometry and opaque Win32 handles for diagnostics and SVG comparison.
pub(super) fn diagnostic_snapshot()->serde_json::Value{SNAPSHOT.with(|value|value.borrow().clone())}

fn error(code:u32,message:&str)->Error{Error::new(HRESULT(code as i32),message)}
fn graphics()->Result<Rc<Gfx>>{GRAPHICS.with(|slot|{
    if let Some(gfx)=slot.borrow().as_ref(){return Ok(gfx.clone());}
    let gfx=Rc::new(Gfx::new(&crate::gfx::system_ui_font(None).0)?);*slot.borrow_mut()=Some(gfx.clone());Ok(gfx)
})}

struct Palette{background:Color,border:Color,text:Color,secondary:Color,disabled:Color,hover:Color,pressed:Color,divider:Color,shadow:f32}
fn palette(dark:bool)->Palette{
    if dark{Palette{background:Color::rgba8(44,44,44,255),border:Color::rgba8(255,255,255,23),text:Color::rgba8(255,255,255,255),
        secondary:Color::rgba8(255,255,255,197),disabled:Color::rgba8(255,255,255,93),hover:Color::rgba8(255,255,255,15),pressed:Color::rgba8(255,255,255,10),divider:Color::rgba8(255,255,255,21),shadow:0.34}}
    else{Palette{background:Color::rgba8(252,252,252,255),border:Color::rgba8(0,0,0,16),text:Color::rgba8(0,0,0,228),
        secondary:Color::rgba8(0,0,0,158),disabled:Color::rgba8(0,0,0,92),hover:Color::rgba8(0,0,0,11),pressed:Color::rgba8(0,0,0,16),divider:Color::rgba8(0,0,0,18),shadow:0.16}}
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]enum Action{Cut,Copy,Rename,Share,Delete}
impl Action{
    const ORDER:[Self;5]=[Self::Cut,Self::Copy,Self::Rename,Self::Share,Self::Delete];
    fn from_verb(verb:&str)->Option<Self>{match verb.to_ascii_lowercase().as_str(){"cut"=>Some(Self::Cut),"copy"=>Some(Self::Copy),"rename"=>Some(Self::Rename),"windows.modernshare"=>Some(Self::Share),"delete"=>Some(Self::Delete),_=>None}}
    fn glyph(self)->&'static str{match self{Self::Cut=>"\u{E8C6}",Self::Copy=>"\u{E8C8}",Self::Rename=>"\u{E8AC}",Self::Share=>"\u{E72D}",Self::Delete=>"\u{E74D}"}}
}
#[derive(Clone)]struct Item{
    position:u32,id:u32,raw_label:String,label:String,mnemonic:Option<char>,accelerator:String,kind:RowKind,enabled:bool,checked:bool,radio:bool,default:bool,
    submenu:HMENU,item_data:usize,owner_draw:bool,bitmap:HBITMAP,measured:Option<(u32,u32)>,wic:Option<IWICBitmapSource>,image:Option<(u64,ID2D1Bitmap)>,action:Option<Action>,
}
impl Item{fn cascade(&self)->bool{!self.submenu.is_invalid()}fn selectable(&self)->bool{self.kind==RowKind::Item&&self.enabled}}
struct Level{menu:HMENU,hwnd:HWND,body:Bounds,metrics:Metrics,layout:LevelLayout,items:Vec<Item>,scroll:f32,
    toolbar:Vec<usize>,accelerator_width:f32,keyboard_focus:bool,
    highlight:Option<usize>,pressed:Option<usize>,open_child:Option<usize>,surface:Option<Surface>,initialized:bool,native_only:bool}
impl Level{
    fn toolbar_height(&self)->f32{if self.toolbar.is_empty(){0.}else{self.metrics.px(TOOLBAR_HEIGHT_DIP)}}
    fn toolbar_rect(&self,slot:usize)->RectF{let m=self.metrics;let pad=m.px(TOOLBAR_PADDING_DIP);let separator=m.px(1.);
        let width=((self.body.width()as f32-2.*pad-separator*(self.toolbar.len().saturating_sub(1))as f32)/self.toolbar.len().max(1)as f32).max(1.);
        RectF::new(pad+slot as f32*(width+separator),m.px(2.),width,m.px(TOOLBAR_HEIGHT_DIP-4.))}
    fn max_scroll(&self)->f32{(self.layout.content_height-self.body.height()as f32).max(0.)}
    fn row_at(&self,point:POINT)->Option<usize>{if !self.body.contains(point.x,point.y){return None;}
        let(x,y)=((point.x-self.body.left)as f32,(point.y-self.body.top)as f32);
        if y<self.toolbar_height(){return self.toolbar.iter().enumerate().find(|(slot,_)|{let r=self.toolbar_rect(*slot);x>=r.x&&x<r.right()&&y>=r.y&&y<r.bottom()}).map(|(_,row)|*row);}
        self.layout.row_at(y,self.scroll,self.metrics).filter(|row|self.items[*row].kind==RowKind::Item&&self.layout.rows[*row].h>0.)}
    fn body_order(&self)->Vec<usize>{self.items.iter().enumerate().filter(|(index,item)|item.selectable()&&self.layout.rows[*index].h>0.).map(|(index,_)|index).collect()}
    fn visual_order(&self)->Vec<usize>{self.toolbar.iter().copied().filter(|row|self.items[*row].selectable()).chain(self.body_order()).collect()}
    fn ensure_visible(&mut self,row:usize){if self.toolbar.contains(&row){return;}if let Some(rect)=self.layout.rows.get(row){let padding=self.metrics.px(geometry::PADDING_DIP);let top=self.toolbar_height()+padding;
        if rect.y-self.scroll<top{self.scroll=(rect.y-top).max(0.);}
        if rect.y+rect.h-self.scroll>self.body.height()as f32-padding{self.scroll=(rect.y+rect.h-self.body.height()as f32+padding).min(self.max_scroll());}}}
}

/// Do not guess which duplicate or provider-owned drawing is the standard
/// action. Ambiguous commands retain their original body rows and callbacks.
fn toolbar_items(items:&[Item])->Vec<usize>{Action::ORDER.iter().filter_map(|action|{
    let candidates:Vec<_>=items.iter().enumerate().filter(|(_,item)|item.action==Some(*action)).map(|(index,_)|index).collect();
    (candidates.len()==1).then(||candidates[0]).filter(|index|{let item=&items[*index];item.kind==RowKind::Item&&!item.cascade()&&!item.owner_draw&&item.bitmap!=HBMMENU_CALLBACK&&!item.label.is_empty()})
}).collect()}
fn next_in_order(order:&[usize],from:Option<usize>,direction:isize)->Option<usize>{if order.is_empty(){return None;}
    let at=from.and_then(|row|order.iter().position(|value|*value==row)).map(|at|at as isize).unwrap_or(if direction>0{-1}else{order.len()as isize});
    Some(order[(at+direction).rem_euclid(order.len()as isize)as usize])}
fn place_shell_submenu(parent:Bounds,top:i32,width:i32,height:i32,work:Bounds,metrics:Metrics)->(i32,i32){
    let overlap=metrics.px(SHELL_SUBMENU_OVERLAP_DIP)as i32;
    let x=if parent.right-overlap+width>work.right{parent.left+overlap-width}else{parent.right-overlap};
    (x.min(work.right-width).max(work.left),(top-metrics.px(geometry::PADDING_DIP)as i32).min(work.bottom-height).max(work.top))
}

struct Provider{context:IContextMenu,options:ContextMenuOptions,root:HMENU,filter:bool,new_root:bool}
impl Provider{
    fn is_new_popup(&self,menu:HMENU)->bool{
        if menu==self.root{return self.new_root;}
        super::menu_catalog::popup_path(self.root,menu,&self.context)
            .is_some_and(|path|path.len()==1&&path[0]=="verb:newfolder")
    }
    /// A callback is delivered to exactly one highest supported Shell interface.
    fn message(&self,message:u32,wp:WPARAM,lp:LPARAM)->Option<LRESULT>{
        if let Ok(handler)=self.context.cast::<IContextMenu3>(){let mut value=LRESULT(0);return unsafe{handler.HandleMenuMsg2(message,wp,lp,Some(&mut value))}.ok().map(|_|value);}
        if let Ok(handler)=self.context.cast::<IContextMenu2>(){return unsafe{handler.HandleMenuMsg(message,wp,lp)}.ok().map(|_|LRESULT(0));}None
    }
    fn filter(&self,menu:HMENU){if !self.filter{return;}
        if menu==self.root{super::filter_native_menu(menu,&self.context,&self.options);}
        else if let Some(path)=super::menu_catalog::popup_path(self.root,menu,&self.context){super::filter_native_menu_at(menu,&self.context,&self.options,&path,false);}}
    fn initialize(&self,menu:HMENU,index:u32){self.message(WM_INITMENUPOPUP,WPARAM(menu.0 as usize),LPARAM(index as isize));self.filter(menu);}
    fn uninitialize(&self,menu:HMENU){self.message(WM_UNINITMENUPOPUP,WPARAM(menu.0 as usize),LPARAM(0));}
    fn selection(&self,level:&Level,row:Option<usize>,mouse:bool){
        if let Some(item)=row.and_then(|row|level.items.get(row)){
            let id=if item.cascade(){item.position}else{item.id};let flags=(if item.cascade(){MF_POPUP.0}else{0})|MF_HILITE.0
                |if mouse{MF_MOUSESELECT.0}else{0}|if item.enabled{0}else{MF_GRAYED.0};
            self.message(WM_MENUSELECT,WPARAM(((flags as usize)<<16)|(id as usize&0xffff)),LPARAM(level.menu.0 as isize));
        }
    }
}

fn valid_bitmap(bitmap:HBITMAP)->bool{(bitmap.0 as usize)>11&&bitmap!=HBMMENU_CALLBACK}
fn shell_label(raw:&str)->(String,Option<char>,String){
    let(mut label,mnemonic,accelerator)=geometry::parse_label(raw);
    let title=raw.split('\t').next().unwrap_or(raw).trim_end();
    if let Some((_,suffix))=title.strip_suffix(')').and_then(|value|value.rsplit_once("(&")){
        if suffix.chars().count()==1{let suffix=format!("({suffix})");
            if let Some(text)=label.strip_suffix(&suffix){label=text.trim_end().into();}}
    }
    (label,mnemonic,accelerator)
}
fn bitmap_alpha(bitmap:HBITMAP)->BitmapAlpha{
    let mut info=DIBSECTION::default();let size=unsafe{GetObjectW(bitmap.into(),std::mem::size_of::<DIBSECTION>()as i32,Some((&mut info as*mut DIBSECTION).cast()))};
    if size as usize==std::mem::size_of::<DIBSECTION>()&&info.dsBm.bmBitsPixel==32&&!info.dsBm.bmBits.is_null(){
        let bytes=(info.dsBm.bmWidthBytes as i64).unsigned_abs().saturating_mul((info.dsBm.bmHeight as i64).unsigned_abs());
        if bytes<=4*1024*1024{let mut pixels=vec![0u8;bytes as usize];
            // Copy through GDI instead of dereferencing a provider-owned DIB.
            if unsafe{GetBitmapBits(bitmap,bytes as i32,pixels.as_mut_ptr().cast())}==bytes as i32&&pixels.chunks_exact(4).any(|pixel|pixel[3]!=0){return BitmapAlpha::Premultiplied;}}
    }BitmapAlpha::Ignore
}
fn read_raw(menu:HMENU,position:u32)->Result<(MENUITEMINFOW,String)>{
    let mut info=MENUITEMINFOW{cbSize:std::mem::size_of::<MENUITEMINFOW>()as u32,
        fMask:MIIM_ID|MIIM_FTYPE|MIIM_STATE|MIIM_SUBMENU|MIIM_DATA|MIIM_BITMAP|MIIM_CHECKMARKS|MIIM_STRING,..Default::default()};
    unsafe{GetMenuItemInfoW(menu,position,true,&mut info)}?;
    if info.cch>32767{return Err(error(0x8007000e,"菜单文字超过可绘制范围"));}
    let mut buffer=vec![0u16;info.cch as usize+1];info.dwTypeData=PWSTR(buffer.as_mut_ptr());info.cch=buffer.len()as u32;
    unsafe{GetMenuItemInfoW(menu,position,true,&mut info)}?;
    let label=String::from_utf16_lossy(&buffer[..buffer.iter().position(|value|*value==0).unwrap_or(buffer.len())]);
    info.dwTypeData=PWSTR::null();Ok((info,label))
}
fn bitmap_for(info:&MENUITEMINFOW)->HBITMAP{
    if info.fState.contains(MFS_CHECKED)&&!info.hbmpChecked.is_invalid(){info.hbmpChecked}
    else if !info.fState.contains(MFS_CHECKED)&&!info.hbmpUnchecked.is_invalid(){info.hbmpUnchecked}else{info.hbmpItem}
}
fn same_identity(item:&Item,info:&MENUITEMINFOW,raw:&str)->bool{
    item.id==info.wID&&item.submenu==info.hSubMenu&&item.item_data==info.dwItemData&&item.raw_label==raw
        &&item.owner_draw==info.fType.contains(MFT_OWNERDRAW)
        &&(item.kind==RowKind::Separator)==info.fType.contains(MFT_SEPARATOR)
}
fn read_items(provider:&Provider,menu:HMENU,gfx:&Gfx,metrics:Metrics)->Result<(Vec<Item>,bool)>{
    let count=unsafe{GetMenuItemCount(Some(menu))};if count<0{return Err(Error::from_thread());}
    if count as usize>MAX_ITEMS{return Ok((Vec::new(),true));}
    let mut items=Vec::with_capacity(count as usize);let mut native_only=false;let mut label_bytes=0usize;
    for position in 0..count as u32{
        let previously_native_only=native_only;
        let(info,raw)=match read_raw(menu,position){Ok(value)=>value,Err(error) if error.code().0 as u32==0x8007000e=>return Ok((Vec::new(),true)),Err(error)=>return Err(error)};
        label_bytes=label_bytes.saturating_add(raw.len());if label_bytes>8*1024*1024{return Ok((Vec::new(),true));}
        let (label,mnemonic,accelerator)=shell_label(&raw);
        let kind=if info.fType.contains(MFT_SEPARATOR){RowKind::Separator}else{RowKind::Item};
        let owner_draw=info.fType.contains(MFT_OWNERDRAW);
        native_only|=info.fType.0&(MFT_MENUBREAK|MFT_MENUBARBREAK|MFT_RIGHTORDER).0!=0;
        let checked=info.fState.contains(MFS_CHECKED);
        let bitmap=bitmap_for(&info);
        if label.is_empty()&&valid_bitmap(bitmap)&&!owner_draw{native_only=true;}
        if !bitmap.is_invalid()&&!valid_bitmap(bitmap)&&bitmap!=HBMMENU_CALLBACK{native_only=true;}
        let mut measured=None;
        if owner_draw{let mut measure=MEASUREITEMSTRUCT{CtlType:ODT_MENU,itemID:info.wID,itemWidth:0,itemHeight:0,itemData:info.dwItemData,..Default::default()};
            if provider.message(WM_MEASUREITEM,WPARAM(0),LPARAM((&mut measure as*mut MEASUREITEMSTRUCT)as isize)).is_none(){native_only=true;}
            else if measure.itemWidth>8192||measure.itemHeight>8192{native_only=true;}
            else{measured=Some((measure.itemWidth,measure.itemHeight.max(metrics.px(geometry::ITEM_HEIGHT_DIP)as u32)));}}
        let mut bitmap_info=BITMAP::default();let bounded_bitmap=!valid_bitmap(bitmap)||{
            let bytes=unsafe{GetObjectW(bitmap.into(),std::mem::size_of::<BITMAP>()as i32,Some((&mut bitmap_info as*mut BITMAP).cast()))};
            bytes as usize==std::mem::size_of::<BITMAP>()&&i64::from(bitmap_info.bmWidth).unsigned_abs().saturating_mul(i64::from(bitmap_info.bmHeight).unsigned_abs()).saturating_mul(4)<=8*1024*1024};
        let wic=if valid_bitmap(bitmap)&&bounded_bitmap{gfx.wic_from_hbitmap(bitmap,bitmap_alpha(bitmap))}else{None};
        if valid_bitmap(bitmap)&&wic.is_none(){native_only=true;}
        if native_only&&!previously_native_only{eprintln!("drawn-menu native row: position={position} type={} bitmap={} owner_draw={owner_draw} measured={measured:?} bitmap_size={}x{} decoded={}",info.fType.0,bitmap.0 as isize,bitmap_info.bmWidth,bitmap_info.bmHeight,wic.is_some());}
        let action=if menu==provider.root&&kind==RowKind::Item{super::canonical_verb(&provider.context,info.wID).as_deref().and_then(Action::from_verb)}else{None};
        items.push(Item{position,id:info.wID,raw_label:raw,label,mnemonic,accelerator,kind,enabled:info.fState.0&(MFS_DISABLED|MFS_GRAYED).0==0,checked,
            radio:info.fType.contains(MFT_RADIOCHECK),default:info.fState.contains(MFS_DEFAULT),submenu:info.hSubMenu,item_data:info.dwItemData,
            owner_draw,bitmap,measured,wic,image:None,action});
    }
    Ok((items,native_only))
}

fn register_class()->Result<()>{
    static REGISTERED:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    let ok=*REGISTERED.get_or_init(||{let Ok(module)=(unsafe{GetModuleHandleW(None)})else{return false;};
        unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(procedure),hInstance:module.into(),lpszClassName:CLASS,hCursor:LoadCursorW(None,IDC_ARROW).unwrap_or_default(),..Default::default()})!=0}});
    if ok{Ok(())}else{Err(error(0x80004005,"无法创建菜单窗口"))}
}
unsafe extern "system" fn procedure(hwnd:HWND,msg:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    match msg{WM_MOUSEACTIVATE=>LRESULT(MA_NOACTIVATE as isize),
        WM_CAPTURECHANGED|WM_CANCELMODE|WM_CLOSE=>{if !NATIVE_TRACKING.with(Cell::get){cancel();}LRESULT(0)},
        _=>unsafe{DefWindowProcW(hwnd,msg,wp,lp)},}
}
fn monitor_at(point:POINT)->(Bounds,u32){let monitor=unsafe{MonitorFromPoint(point,MONITOR_DEFAULTTONEAREST)};
    let mut info=MONITORINFO{cbSize:std::mem::size_of::<MONITORINFO>()as u32,..Default::default()};
    let work=if unsafe{GetMonitorInfoW(monitor,&mut info)}.as_bool(){Bounds::new(info.rcWork.left,info.rcWork.top,info.rcWork.right,info.rcWork.bottom)}
        else{Bounds::new(0,0,unsafe{GetSystemMetrics(SM_CXSCREEN)},unsafe{GetSystemMetrics(SM_CYSCREEN)})};
    let(mut x,mut y)=(96,96);let dpi=if unsafe{GetDpiForMonitor(monitor,MDT_EFFECTIVE_DPI,&mut x,&mut y)}.is_ok(){x.max(96)}else{96};(work,dpi)}
enum Place{Root(POINT),Beside{parent:Bounds,top:i32}}
fn measure_items(items:&[Item],gfx:&Gfx,metrics:Metrics,new_popup:bool)->(LevelLayout,Vec<usize>,f32){
    let style=TextStyle::ui(metrics.px(geometry::TEXT_DIP));let accelerator=TextStyle::ui(metrics.px(geometry::ACCELERATOR_DIP));
    let measured:Vec<_>=items.iter().map(|item|Measured{label:if item.owner_draw{0.}else{gfx.measure(&item.label,&style.weight(if item.default{600}else{400}),4096.).0},
        accelerator:if item.accelerator.is_empty(){0.}else{gfx.measure(&item.accelerator,&accelerator,4096.).0}}).collect();
    let kinds:Vec<_>=items.iter().map(|item|item.kind).collect();
    let toolbar=toolbar_items(items);
    let mut layout=geometry::layout(&kinds,&measured,true,items.iter().any(Item::cascade),metrics);
    let header=if toolbar.is_empty(){0.}else{metrics.px(TOOLBAR_HEIGHT_DIP)};
    let accelerator_width=items.iter().enumerate().filter(|(index,item)|!toolbar.contains(index)&&!item.owner_draw&&item.kind==RowKind::Item).map(|(index,_)|measured[index].accelerator).fold(0.,f32::max);
    let mut y=header+metrics.px(geometry::PADDING_DIP);let mut width=metrics.px(geometry::MIN_WIDTH_DIP);let mut previous_item=false;
    for(index,(row,item))in layout.rows.iter_mut().zip(items).enumerate(){row.y=y;
        // Zero-height entries remain in the model at their original positions.
        // Only promoted actions and now-empty separator groups leave the body.
        if toolbar.contains(&index){row.h=0.;continue;}
        if item.kind==RowKind::Separator{
            if new_popup{row.h=0.;continue;}
            let followed=items.iter().enumerate().skip(index+1).any(|(next,item)|item.kind==RowKind::Item&&!toolbar.contains(&next));
            if !previous_item||!followed{row.h=0.;continue;}previous_item=false;
        }else{
            previous_item=true;
            let trailing=metrics.px(geometry::CONTENT_RIGHT_DIP+geometry::INSET_DIP)+if item.cascade(){metrics.px(geometry::CHEVRON_DIP)}else{0.};
            let extra=if measured[index].accelerator>0.{metrics.px(geometry::ACCELERATOR_GAP_DIP)+accelerator_width}else{0.};
            width=width.max(metrics.px(SHELL_TEXT_LEFT_DIP)+measured[index].label+extra+trailing);
        }
        if let Some((owned_width,height))=item.measured{row.h=row.h.max(height as f32);width=width.max(owned_width as f32+2.*metrics.px(geometry::INSET_DIP));}y+=row.h;
    }
    layout.width=width.ceil().min(metrics.px(geometry::MAX_WIDTH_DIP));
    if !toolbar.is_empty(){layout.width=layout.width.max(metrics.px(TOOLBAR_BUTTON_DIP)*toolbar.len()as f32+metrics.px(1.)*(toolbar.len()-1)as f32+2.*metrics.px(TOOLBAR_PADDING_DIP));}
    layout.content_height=y+metrics.px(geometry::PADDING_DIP);(layout,toolbar,accelerator_width)
}
fn open_level(provider:&Provider,gfx:&Gfx,owner:HWND,menu:HMENU,index:u32,place:Place,initialized:bool)->Result<Option<Level>>{
    let reference=match place{Place::Root(point)=>point,Place::Beside{parent,top}=>POINT{x:parent.right,y:top}};
    let(work,dpi)=monitor_at(reference);let metrics=Metrics::new(dpi);
    if !initialized{provider.initialize(menu,index);}
    let result=(||{
        let(items,mut native_only)=read_items(provider,menu,gfx,metrics)?;
        if items.is_empty()&&!native_only{return Ok(None);}
        let(mut layout,toolbar,accelerator_width)=measure_items(&items,gfx,metrics,provider.is_new_popup(menu));
        let width=(layout.width.ceil()as i32).min(work.width().max(1));layout.width=width as f32;let height=geometry::body_height(&layout,work);
        native_only|=!toolbar.is_empty()&&height<metrics.px(TOOLBAR_HEIGHT_DIP+geometry::ITEM_HEIGHT_DIP+2.*geometry::PADDING_DIP)as i32;
        let(x,y)=match place{Place::Root(point)=>geometry::place_root(Anchor::Point{x:point.x,y:point.y},width,height,work,metrics),
            Place::Beside{parent,top}=>place_shell_submenu(parent,top,width,height,work,metrics)};
        let shadow=metrics.px(geometry::SHADOW_DIP)as i32;let module=unsafe{GetModuleHandleW(None)}?;
        native_only|=(i64::from(width)+2*i64::from(shadow))*(i64::from(height)+2*i64::from(shadow))*4>64*1024*1024;
        let hwnd=unsafe{CreateWindowExW(WS_EX_LAYERED|WS_EX_TOOLWINDOW|WS_EX_TOPMOST|WS_EX_NOACTIVATE,CLASS,windows::core::PCWSTR(zsdesk_core::i18n::wide("右键菜单").as_ptr()),WS_POPUP,
            x-shadow,y-shadow,width+shadow*2,height+shadow*2,Some(owner),None,Some(module.into()),None)}?;
        Ok(Some(Level{menu,hwnd,body:Bounds::new(x,y,x+width,y+height),metrics,layout,items,toolbar,accelerator_width,keyboard_focus:false,scroll:0.,highlight:None,pressed:None,open_child:None,surface:None,initialized:true,native_only}))
    })();
    if !matches!(result,Ok(Some(_))){provider.uninitialize(menu);}
    result
}

fn action_label(item:&Item)->String{
    let raw=item.raw_label.split_once('\t').map_or(item.raw_label.as_str(),|(label,_)|label).trim();
    let label=raw.rfind("(&").filter(|start|raw.ends_with(')')&&raw[*start+2..raw.len()-1].chars().count()==1).map_or(raw,|start|raw[..start].trim_end());
    geometry::parse_label(label).0
}
fn paint_toolbar(cv:&Canvas,level:&Level,colors:&Palette,body:RectF){
    let m=level.metrics;
    for(slot,index)in level.toolbar.iter().enumerate(){let item=&level.items[*index];let rect=level.toolbar_rect(slot);let plate=RectF::new(body.x+rect.x,body.y+rect.y,rect.w,rect.h);
        let active=level.highlight==Some(*index);let color=if item.enabled{colors.text}else{colors.disabled};
        if active&&item.enabled{cv.round(plate.inset(m.px(1.),m.px(1.)),m.px(4.),if level.pressed==Some(*index){colors.pressed}else{colors.hover});
            if level.keyboard_focus{cv.stroke_round(plate.inset(m.px(2.),m.px(2.)),m.px(3.),colors.secondary,m.px(1.));}}
        let side=m.px(geometry::ICON_DIP);let icon=RectF::new((plate.x+(plate.w-side)/2.).round(),body.y+m.px(12.),side,side);
        if let Some((_,image))=&item.image{cv.image(image,icon,if item.enabled{1.}else{0.45});}
        else if let Some(action)=item.action{cv.glyph(action.glyph(),icon,side,color);}
        if item.checked{let badge=RectF::new(plate.right()-m.px(11.),body.y+m.px(5.),m.px(8.),m.px(8.));
            if item.radio{cv.round(badge.inset(m.px(2.),m.px(2.)),m.px(2.),color);}else{cv.glyph("\u{E73E}",badge,m.px(8.),color);}}
        cv.text(&action_label(item),RectF::new(plate.x+m.px(2.),body.y+m.px(34.),plate.w-m.px(4.),m.px(20.)),
            &TextStyle::ui(m.px(TOOLBAR_LABEL_DIP)).align(Align::Center).weight(if item.default{600}else{400}),color);
        if slot+1<level.toolbar.len(){let x=plate.right()+m.px(0.5);cv.line(x,body.y+m.px(12.),x,body.y+m.px(48.),colors.divider,m.px(1.));}
    }
    if !level.toolbar.is_empty(){let y=body.y+level.toolbar_height();cv.line(body.x+m.px(SHELL_SEPARATOR_INSET_DIP),y,body.right()-m.px(SHELL_SEPARATOR_INSET_DIP),y,colors.divider,m.px(1.));}
}
fn paint(cv:&Canvas,level:&Level,colors:&Palette){
    let m=level.metrics;let shadow=m.px(geometry::SHADOW_DIP);let body=RectF::new(shadow,shadow,level.body.width()as f32,level.body.height()as f32);let radius=m.px(geometry::RADIUS_DIP);
    let offset=m.px(geometry::SHADOW_OFFSET_DIP);let layers=(shadow-offset).max(1.)as i32;let alpha=1.-(1.-colors.shadow).powf(1./layers as f32);
    for grow in (1..=layers).rev(){let g=grow as f32;cv.round(RectF::new(body.x-g,body.y-g+offset,body.w+2.*g,body.h+2.*g),radius+g,Color::rgba8(0,0,0,255).with_alpha(alpha));}
    cv.round(body,radius,colors.background);cv.stroke_round(body.inset(0.5,0.5),(radius-0.5).max(0.),colors.border,1.);
    cv.push_clip(body.inset(1.,1.));paint_toolbar(cv,level,colors,body);cv.pop_clip();
    let header=level.toolbar_height();cv.push_clip(RectF::new(body.x+1.,body.y+header+1.,body.w-2.,(body.h-header-2.).max(0.)));
    let inset=m.px(geometry::INSET_DIP);let text_left=body.x+m.px(SHELL_TEXT_LEFT_DIP);
    let accelerator_left=body.right()-m.px(geometry::CONTENT_RIGHT_DIP+geometry::INSET_DIP)-level.accelerator_width;
    for(index,(row,item))in level.layout.rows.iter().zip(&level.items).enumerate(){let y=body.y+row.y-level.scroll;if row.h<=0.||y+row.h<body.y+header||y>body.bottom(){continue;}
        if item.kind==RowKind::Separator{let line=(y+row.h/2.).floor()+0.5;cv.line(body.x+m.px(SHELL_SEPARATOR_INSET_DIP),line,body.right()-m.px(SHELL_SEPARATOR_INSET_DIP),line,colors.divider,1.);continue;}
        let active=level.highlight==Some(index)||level.open_child==Some(index);let plate=RectF::new(body.x+inset,y+m.px(2.),body.w-2.*inset,row.h-m.px(4.));
        if active&&item.enabled{cv.round(plate,m.px(4.),if level.pressed==Some(index){colors.pressed}else{colors.hover});}
        if active&&item.enabled&&level.keyboard_focus{cv.stroke_round(plate.inset(1.,1.),m.px(3.),colors.secondary,1.);}
        if item.owner_draw{continue;}
        let color=if item.enabled{colors.text}else{colors.disabled};let icon=RectF::new(body.x+inset+m.px(geometry::CONTENT_LEFT_DIP),y,m.px(geometry::ICON_DIP),row.h);
        if item.checked&&item.wic.is_none(){if item.radio{let side=m.px(6.);cv.round(RectF::new(icon.x+(icon.w-side)/2.,y+(row.h-side)/2.,side,side),side/2.,color);}else{cv.glyph("\u{E73E}",icon,m.px(12.),color);}}
        else if let Some((_,image))=&item.image{let side=m.px(geometry::ICON_DIP);cv.image(image,RectF::new(icon.x,y+(row.h-side)/2.,side,side),if item.enabled{1.}else{0.45});}
        else if let Some(action)=item.action{cv.glyph(action.glyph(),icon,m.px(geometry::ICON_DIP),color);}
        let trailing=m.px(geometry::CONTENT_RIGHT_DIP)+if item.cascade(){m.px(geometry::CHEVRON_DIP)}else{0.};let right=plate.right()-trailing;
        let label_right=if item.accelerator.is_empty(){right}else{accelerator_left-m.px(geometry::ACCELERATOR_GAP_DIP)};
        cv.text(&item.label,RectF::new(text_left,y,(label_right-text_left).max(0.),row.h),&TextStyle::ui(m.px(geometry::TEXT_DIP)).weight(if item.default{600}else{400}),color);
        if !item.accelerator.is_empty(){cv.text(&item.accelerator,RectF::new(accelerator_left,y,(right-accelerator_left).max(0.),row.h),&TextStyle::ui(m.px(geometry::ACCELERATOR_DIP)),if item.enabled{colors.secondary}else{colors.disabled});}
        if item.cascade(){cv.glyph("\u{E76C}",RectF::new(plate.right()-m.px(geometry::CONTENT_RIGHT_DIP+12.),y,m.px(12.),row.h),m.px(10.),color);}
    }
    if level.max_scroll()>0.{let pad=m.px(geometry::PADDING_DIP);let visible=(body.h-header).max(1.);let track=(visible-2.*pad).max(1.);let thumb=(track*visible/(level.layout.content_height-header).max(1.)).max(m.px(24.)).min(track);
        let top=body.y+header+pad+(track-thumb)*level.scroll/level.max_scroll();cv.round(RectF::new(body.right()-m.px(5.),top,m.px(2.),thumb),m.px(1.),colors.secondary.with_alpha(0.5));}
    cv.pop_clip();
}
fn paint_provider_rows(provider:&Provider,level:&Level,surface:&Surface){
    let shadow=level.metrics.px(geometry::SHADOW_DIP)as i32;
    for(index,(row,item))in level.layout.rows.iter().zip(&level.items).enumerate(){if !item.owner_draw&&item.bitmap!=HBMMENU_CALLBACK{continue;}
        // Providers may replace an owner-draw payload while a menu is open.
        // Never pass a stale itemData pointer back to the provider's drawer.
        let Ok((current,label))=read_raw(level.menu,item.position)else{continue;};if !same_identity(item,&current,&label){continue;}
        let top=shadow+(row.y-level.scroll).floor()as i32;let bottom=top+row.h.ceil()as i32;
        if bottom<=shadow||top>=shadow+level.body.height(){continue;}
        let mut rect=RECT{left:shadow+level.metrics.px(geometry::INSET_DIP)as i32,top,right:shadow+level.body.width()-level.metrics.px(geometry::INSET_DIP)as i32,bottom};
        if !item.owner_draw{rect.left+=level.metrics.px(geometry::CONTENT_LEFT_DIP)as i32;rect.right=rect.left+level.metrics.px(geometry::ICON_DIP)as i32;let side=rect.right-rect.left;rect.top+=(rect.bottom-rect.top-side)/2;rect.bottom=rect.top+side;}
        let clip=RECT{left:rect.left.max(shadow+1),top:rect.top.max(shadow+level.toolbar_height()as i32+1),right:rect.right.min(shadow+level.body.width()-1),bottom:rect.bottom.min(shadow+level.body.height()-1)};
        if clip.right<=clip.left||clip.bottom<=clip.top{continue;}
        let mut flags=ODS_NOFOCUSRECT;if level.highlight==Some(index)||level.open_child==Some(index){flags.0|=ODS_SELECTED.0;}
        if !item.enabled{flags.0|=ODS_DISABLED.0|ODS_GRAYED.0;}if item.checked{flags.0|=ODS_CHECKED.0;}if item.default{flags.0|=ODS_DEFAULT.0;}
        let mut draw=DRAWITEMSTRUCT{CtlType:ODT_MENU,itemID:item.id,itemAction:ODA_DRAWENTIRE,itemState:flags,hwndItem:HWND(level.menu.0),hDC:surface.dc,rcItem:rect,itemData:item.item_data,..Default::default()};
        let saved=unsafe{SaveDC(surface.dc)};unsafe{IntersectClipRect(surface.dc,clip.left,clip.top,clip.right,clip.bottom);SetBkMode(surface.dc,TRANSPARENT);}
        provider.message(WM_DRAWITEM,WPARAM(0),LPARAM((&mut draw as*mut DRAWITEMSTRUCT)as isize));
        if saved!=0{let _=unsafe{RestoreDC(surface.dc,saved)};}let _=unsafe{GdiFlush()};
        // Ordinary GDI drawers write RGB and leave alpha undefined. Only their
        // clipped row becomes opaque; the flyout's rounded shadow stays intact.
        let pixels=unsafe{std::slice::from_raw_parts_mut(surface.bits,surface.pixel_bytes()as usize)};
        for y in clip.top.max(0)..clip.bottom.min(surface.height){for x in clip.left.max(0)..clip.right.min(surface.width){pixels[((y*surface.width+x)*4+3)as usize]=255;}}
    }
}
fn present(provider:&Provider,level:&mut Level,gfx:&Gfx,colors:&Palette)->Result<()>{
    for item in &mut level.items{if item.image.as_ref().is_none_or(|(generation,_)|*generation!=gfx.generation()){item.image=item.wic.as_ref().and_then(|wic|gfx.upload(wic)).map(|image|(gfx.generation(),image));}}
    let shadow=level.metrics.px(geometry::SHADOW_DIP)as i32;let(width,height)=(level.body.width()+2*shadow,level.body.height()+2*shadow);
    let mut surface=level.surface.take();let target=Surface::ensure(&mut surface,width,height).ok_or_else(||error(0x8007000e,"菜单绘图内存不足"))?;
    let drawn=gfx.draw(target,|canvas|paint(canvas,level,colors));if drawn.is_ok(){paint_provider_rows(provider,level,target);}
    drawn?;let screen=unsafe{GetDC(None)};let blend=BLENDFUNCTION{BlendOp:AC_SRC_OVER as u8,BlendFlags:0,SourceConstantAlpha:255,AlphaFormat:AC_SRC_ALPHA as u8};
    let origin=POINT::default();let at=POINT{x:level.body.left-shadow,y:level.body.top-shadow};let size=SIZE{cx:width,cy:height};
    let updated=unsafe{UpdateLayeredWindow(level.hwnd,Some(screen),Some(&at),Some(&size),Some(target.dc),Some(&origin),Default::default(),Some(&blend),ULW_ALPHA)};
    unsafe{ReleaseDC(None,screen);}updated?;
    if !unsafe{IsWindowVisible(level.hwnd)}.as_bool(){unsafe{SetWindowPos(level.hwnd,Some(HWND_TOPMOST),0,0,0,0,SWP_NOMOVE|SWP_NOSIZE|SWP_NOACTIVATE|SWP_SHOWWINDOW)}?;}
    level.surface=surface;Ok(())
}

struct NativeForward<'a>{provider:&'a Provider,menu:HMENU,suppress_init:bool,suppress_uninit:bool}
unsafe extern "system" fn native_forward(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM,_id:usize,data:usize)->LRESULT{
    let state=unsafe{&mut*(data as*mut NativeForward<'_>)};
    if message==WM_TIMER&&wp.0==NATIVE_SUBCLASS{let foreground=unsafe{GetForegroundWindow()};
        if !foreground.0.is_null()&&foreground!=hwnd&&unsafe{GetAncestor(foreground,GA_ROOTOWNER)}!=hwnd{let _=unsafe{EndMenu()};}return LRESULT(0);}
    if message==WM_INITMENUPOPUP{let popup=HMENU(wp.0 as*mut _);if popup==state.menu&&state.suppress_init{state.suppress_init=false;}else{state.provider.message(message,wp,lp);}state.provider.filter(popup);return LRESULT(0);}
    if message==WM_UNINITMENUPOPUP&&HMENU(wp.0 as*mut _)==state.menu&&state.suppress_uninit{return LRESULT(0);}
    if matches!(message,WM_INITMENU|WM_UNINITMENUPOPUP|WM_DRAWITEM|WM_MEASUREITEM|WM_MENUCHAR|WM_MENUSELECT){return state.provider.message(message,wp,lp).unwrap_or(LRESULT(0));}
    unsafe{DefSubclassProc(hwnd,message,wp,lp)}
}
fn native_fallback(provider:&Provider,owner:HWND,menu:HMENU,point:POINT,initialized:bool)->Result<Option<u32>>{
    let mut state=NativeForward{provider,menu,suppress_init:initialized,suppress_uninit:initialized};
    if !unsafe{SetWindowSubclass(owner,Some(native_forward),NATIVE_SUBCLASS,(&mut state as*mut NativeForward<'_>)as usize)}.as_bool(){return Err(Error::from_thread());}
    struct Scope(HWND);impl Drop for Scope{fn drop(&mut self){let _=unsafe{KillTimer(Some(self.0),NATIVE_SUBCLASS)};let _=unsafe{RemoveWindowSubclass(self.0,Some(native_forward),NATIVE_SUBCLASS)};let _=unsafe{PostMessageW(Some(self.0),WM_NULL,WPARAM(0),LPARAM(0))};NATIVE_TRACKING.with(|active|active.set(false));}}
    NATIVE_TRACKING.with(|active|active.set(true));let _scope=Scope(owner);if unsafe{GetCapture()}.0.is_null()==false{let _=unsafe{ReleaseCapture()};}
    let _=unsafe{SetForegroundWindow(owner)};unsafe{SetTimer(Some(owner),NATIVE_SUBCLASS,30,None);}
    let command=unsafe{TrackPopupMenuEx(menu,(TPM_RETURNCMD|TPM_RIGHTBUTTON|TPM_LEFTALIGN|TPM_TOPALIGN).0,point.x,point.y,owner,None)}.0 as u32;
    Ok((command!=0).then_some(command))
}

struct Tooltip{hwnd:HWND,owner:HWND,text:Vec<u16>}
impl Tooltip{
    fn new(owner:HWND)->Option<Self>{
        let hwnd=unsafe{CreateWindowExW(WS_EX_TOPMOST|WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,w!("tooltips_class32"),None,
            WINDOW_STYLE(WS_POPUP.0|TTS_ALWAYSTIP|TTS_NOPREFIX),0,0,0,0,Some(owner),None,None,None)}.ok()?;
        let mut tip=Self{hwnd,owner,text:vec![0]};let info=tip.info();
        if unsafe{SendMessageW(hwnd,TTM_ADDTOOLW,Some(WPARAM(0)),Some(LPARAM((&info as*const TTTOOLINFOW)as isize)))}.0==0{return None;}
        unsafe{SendMessageW(hwnd,TTM_SETMAXTIPWIDTH,Some(WPARAM(0)),Some(LPARAM(360)));}Some(tip)
    }
    fn info(&mut self)->TTTOOLINFOW{TTTOOLINFOW{cbSize:std::mem::size_of::<TTTOOLINFOW>()as u32,uFlags:TTF_TRACK|TTF_ABSOLUTE,
        hwnd:self.owner,uId:1,lpszText:PWSTR(self.text.as_mut_ptr()),..Default::default()}}
    fn hide(&mut self){let info=self.info();unsafe{SendMessageW(self.hwnd,TTM_TRACKACTIVATE,Some(WPARAM(0)),Some(LPARAM((&info as*const TTTOOLINFOW)as isize)));}}
    fn show(&mut self,text:&str,point:POINT){self.hide();self.text=text.encode_utf16().chain(Some(0)).collect();let info=self.info();
        let position=(point.x as u16 as u32)|((point.y as u16 as u32)<<16);
        unsafe{SendMessageW(self.hwnd,TTM_UPDATETIPTEXTW,Some(WPARAM(0)),Some(LPARAM((&info as*const TTTOOLINFOW)as isize)));
            SendMessageW(self.hwnd,TTM_TRACKPOSITION,Some(WPARAM(0)),Some(LPARAM(position as isize)));
            SendMessageW(self.hwnd,TTM_TRACKACTIVATE,Some(WPARAM(1)),Some(LPARAM((&info as*const TTTOOLINFOW)as isize)));}}
}
impl Drop for Tooltip{fn drop(&mut self){self.hide();let _=unsafe{DestroyWindow(self.hwnd)};}}
struct Session{provider:Provider,owner:HWND,levels:Vec<Level>,entered:bool,revision:u64,tooltip:Option<Tooltip>,tip_target:Option<(usize,usize,Instant)>,tip_shown:bool}
impl Session{
    fn hide_tooltip(&mut self){if let Some(tip)=&mut self.tooltip{tip.hide();}self.tip_target=None;self.tip_shown=false;}
    fn schedule_tooltip(&mut self,target:Option<(usize,usize)>){
        if self.tip_target.as_ref().map(|(depth,row,_)|(*depth,*row))==target{return;}
        self.hide_tooltip();self.tip_target=target.map(|(depth,row)|(depth,row,Instant::now()));
    }
    fn update_tooltip(&mut self){
        let Some((depth,row,since))=self.tip_target else{return;};if self.tip_shown||since.elapsed()<TOOLTIP_DELAY{return;}
        let Some(level)=self.levels.get(depth)else{self.hide_tooltip();return;};
        let Some(slot)=level.toolbar.iter().position(|index|*index==row)else{self.hide_tooltip();return;};
        let Some(item)=level.items.get(row)else{return;};let rect=level.toolbar_rect(slot);
        let text=if item.accelerator.is_empty(){item.label.clone()}else{format!("{} ({})",item.label,item.accelerator)};
        let point=POINT{x:level.body.left+(rect.x+rect.w/2.)as i32,y:level.body.top+rect.bottom()as i32+level.metrics.px(8.)as i32};
        if self.tooltip.is_none(){self.tooltip=Tooltip::new(self.levels[0].hwnd);}
        if let Some(tip)=&mut self.tooltip{tip.show(&text,point);self.tip_shown=true;}
    }
    fn close_from(&mut self,depth:usize){if self.tip_target.is_some_and(|(at,_,_)|at>=depth){self.hide_tooltip();}while self.levels.len()>depth{if let Some(level)=self.levels.pop(){
        if ROOT.with(Cell::get)==level.hwnd{ROOT.with(|root|root.set(HWND::default()));}
        let _=unsafe{DestroyWindow(level.hwnd)};if level.initialized{self.provider.uninitialize(level.menu);}}}
        if let Some(last)=self.levels.last_mut(){last.open_child=None;}}
    fn snapshot(&self){SNAPSHOT.with(|slot|*slot.borrow_mut()=serde_json::json!({"renderer":"drawn-shell-hmenu","metrics":{"row_dip":geometry::ITEM_HEIGHT_DIP,"separator_dip":geometry::SEPARATOR_HEIGHT_DIP,"text_dip":geometry::TEXT_DIP,"radius_dip":geometry::RADIUS_DIP,"shadow_dip":geometry::SHADOW_DIP,"icon_dip":geometry::ICON_DIP,"toolbar_height_dip":TOOLBAR_HEIGHT_DIP,"toolbar_button_dip":TOOLBAR_BUTTON_DIP,"toolbar_label_dip":TOOLBAR_LABEL_DIP,"text_left_dip":SHELL_TEXT_LEFT_DIP,"submenu_overlap_dip":SHELL_SUBMENU_OVERLAP_DIP},"dark":crate::theme::menus_dark(),"levels":self.levels.iter().map(|level|
        serde_json::json!({"window":level.hwnd.0 as usize,"menu":level.menu.0 as usize,"bounds":[level.body.left,level.body.top,level.body.right,level.body.bottom],
            "dpi":(level.metrics.scale*96.).round()as u32,"scroll":level.scroll,"highlight":level.highlight,"open_child":level.open_child,"native_fallback":level.native_only,"text_left":level.metrics.px(SHELL_TEXT_LEFT_DIP),"keyboard_focus":level.keyboard_focus,"accelerator_width":level.accelerator_width,
            "toolbar":level.toolbar.iter().enumerate().map(|(slot,index)|{let rect=level.toolbar_rect(slot);let item=&level.items[*index];serde_json::json!({"row":index,"id":item.id,"position":item.position,"label":item.label,"enabled":item.enabled,"rect":[rect.x,rect.y,rect.w,rect.h],"action":format!("{:?}",item.action)})}).collect::<Vec<_>>(),
            "rows":level.items.iter().zip(&level.layout.rows).map(|(item,row)|serde_json::json!({"id":item.id,"label":item.label,"position":item.position,"submenu":item.cascade(),"enabled":item.enabled,"checked":item.checked,"radio":item.radio,"default":item.default,"owner_draw":item.owner_draw,"y":row.y,"height":row.h})).collect::<Vec<_>>()})
        ).collect::<Vec<_>>()}));}
    fn present(&mut self,depth:usize,gfx:&Gfx,colors:&Palette)->Result<()>{present(&self.provider,&mut self.levels[depth],gfx,colors)?;self.snapshot();Ok(())}
    fn rebuild_level(&mut self,depth:usize,gfx:&Gfx,colors:&Palette)->Result<Step>{
        self.hide_tooltip();
        let level=&self.levels[depth];let menu=level.menu;let metrics=level.metrics;let body=level.body;
        let selected=level.highlight.and_then(|index|level.items.get(index)).map(|item|(item.id,item.submenu,item.item_data,item.raw_label.clone()));
        let child=self.levels.get(depth+1).map(|level|level.menu);
        self.provider.filter(menu);let(items,native_only)=read_items(&self.provider,menu,gfx,metrics)?;
        if native_only{self.close_from(depth+1);let result=native_fallback(&self.provider,self.owner,menu,POINT{x:body.left,y:body.top},true)?;return Ok(result.map_or(Step::Dismiss,Step::Choose));}
        let child_row=child.and_then(|child|items.iter().position(|item|item.enabled&&item.submenu==child));
        if child.is_some()&&child_row.is_none(){self.close_from(depth+1);}
        let(mut layout,toolbar,accelerator_width)=measure_items(&items,gfx,metrics,self.provider.is_new_popup(menu));let(work,_)=monitor_at(POINT{x:body.left,y:body.top});
        let width=(layout.width.ceil()as i32).min(work.width().max(1));layout.width=width as f32;let height=geometry::body_height(&layout,work);
        let(x,y)=geometry::place_root(Anchor::Point{x:body.left,y:body.top},width,height,work,metrics);
        let highlight=selected.and_then(|key|items.iter().position(|item|item.selectable()&&(item.id,item.submenu,item.item_data,item.raw_label.clone())==key));
        let level=&mut self.levels[depth];level.items=items;level.layout=layout;level.toolbar=toolbar;level.accelerator_width=accelerator_width;level.body=Bounds::new(x,y,x+width,y+height);level.scroll=level.scroll.min(level.max_scroll());
        level.highlight=highlight;level.pressed=None;level.open_child=child_row;
        self.revision=self.revision.wrapping_add(1);
        self.present(depth,gfx,colors)?;
        // Reposition existing children without another initialization callback.
        for index in depth+1..self.levels.len(){
            let parent=&self.levels[index-1];let Some(row)=parent.open_child else{break;};let top=parent.body.top+(parent.layout.rows[row].y-parent.scroll)as i32;
            let parent_body=parent.body;let level=&mut self.levels[index];let(work,_)=monitor_at(POINT{x:parent_body.right,y:top});
            let(width,height)=(level.body.width(),level.body.height());let(x,y)=place_shell_submenu(parent_body,top,width,height,work,level.metrics);
            level.body=Bounds::new(x,y,x+width,y+height);self.present(index,gfx,colors)?;
        }
        Ok(Step::Continue)
    }
    fn refresh_states(&mut self,gfx:&Gfx,colors:&Palette)->Result<Step>{
        let mut depth=0;
        while depth<self.levels.len(){
            let count=unsafe{GetMenuItemCount(Some(self.levels[depth].menu))};if count<0{return Err(error(0x80070006,"扩展菜单已失效"));}
            let mut rebuild=count as usize!=self.levels[depth].items.len();let mut dirty=false;
            if !rebuild{
                let level=&mut self.levels[depth];
                for(index,item)in level.items.iter_mut().enumerate(){
                    let row=&level.layout.rows[index];let visible=level.toolbar.contains(&index)||(row.h>0.&&row.y+row.h>=level.scroll&&row.y<=level.scroll+level.body.height()as f32);
                    if !visible&&level.open_child!=Some(index)&&level.highlight!=Some(index){continue;}
                    let(info,label)=read_raw(level.menu,item.position)?;
                    if !same_identity(item,&info,&label)||item.bitmap!=bitmap_for(&info)||item.radio!=info.fType.contains(MFT_RADIOCHECK)||item.default!=info.fState.contains(MFS_DEFAULT){rebuild=true;break;}
                    let enabled=info.fState.0&(MFS_DISABLED|MFS_GRAYED).0==0;let checked=info.fState.contains(MFS_CHECKED);
                    if enabled!=item.enabled||checked!=item.checked{item.enabled=enabled;item.checked=checked;dirty=true;}
                }
            }
            if rebuild{let result=self.rebuild_level(depth,gfx,colors)?;if !matches!(result,Step::Continue){return Ok(result);}}
            else if dirty{
                if self.levels[depth].open_child.is_some_and(|row|!self.levels[depth].items[row].enabled){self.close_from(depth+1);}
                if self.levels[depth].highlight.is_some_and(|row|!self.levels[depth].items[row].enabled){self.levels[depth].highlight=None;self.levels[depth].pressed=None;}
                self.present(depth,gfx,colors)?;
            }
            depth+=1;
        }
        Ok(Step::Continue)
    }
    fn highlight(&mut self,depth:usize,row:Option<usize>,mouse:bool,gfx:&Gfx,colors:&Palette)->Result<()>{
        let row=row.filter(|row|self.levels[depth].items[*row].selectable());
        if self.levels[depth].highlight!=row||self.levels[depth].pressed.is_some()||self.levels[depth].keyboard_focus==mouse{self.levels[depth].highlight=row;self.levels[depth].pressed=None;self.levels[depth].keyboard_focus=!mouse;
            self.provider.selection(&self.levels[depth],row,mouse);self.present(depth,gfx,colors)?;}
        if !mouse{self.schedule_tooltip(row.filter(|row|self.levels[depth].toolbar.contains(row)).map(|row|(depth,row)));}Ok(())
    }
    fn open_child(&mut self,depth:usize,row:usize,gfx:&Gfx,colors:&Palette,keyboard:bool)->Result<Step>{
        if self.levels[depth].open_child==Some(row)&&self.levels.len()>depth+1{return Ok(Step::Continue);}
        if self.levels.len()>=MAX_LEVELS{return Ok(Step::Continue);}
        let item=self.levels[depth].items[row].clone();if !item.enabled||!item.cascade(){return Ok(Step::Continue);}
        if self.levels.iter().any(|level|level.menu==item.submenu){return Err(error(0x80070057,"菜单子级引用形成循环"));}
        self.close_from(depth+1);let parent=self.levels[depth].body;let top=parent.top+(self.levels[depth].layout.rows[row].y-self.levels[depth].scroll)as i32;
        let Some(mut child)=open_level(&self.provider,gfx,self.owner,item.submenu,item.position,Place::Beside{parent,top},false)?else{self.present(depth,gfx,colors)?;return Ok(Step::Continue);};
        let used:u64=self.levels.iter().filter_map(|level|level.surface.as_ref()).map(Surface::pixel_bytes).sum();let shadow=child.metrics.px(geometry::SHADOW_DIP)as u64;
        let needed=(child.body.width()as u64+2*shadow)*(child.body.height()as u64+2*shadow)*4;child.native_only|=used.saturating_add(needed)>128*1024*1024;
        self.levels[depth].open_child=Some(row);self.levels.push(child);
        if CANCEL.with(Cell::get)||!unsafe{IsWindow(Some(self.owner))}.as_bool(){return Ok(Step::Dismiss);}
        self.present(depth,gfx,colors)?;
        let child_depth=self.levels.len()-1;
        if self.levels[child_depth].native_only{
            let body=self.levels[child_depth].body;let selected=native_fallback(&self.provider,self.owner,item.submenu,POINT{x:body.left,y:body.top},true)?;
            self.close_from(child_depth);return Ok(selected.map_or(Step::Dismiss,Step::Choose));
        }
        self.present(child_depth,gfx,colors)?;
        if keyboard{let row=next_in_order(&self.levels[child_depth].visual_order(),None,1);self.highlight(child_depth,row,false,gfx,colors)?;}
        Ok(Step::Continue)
    }
    fn activate(&mut self,depth:usize,row:usize,gfx:&Gfx,colors:&Palette,keyboard:bool)->Result<Step>{
        let item=&self.levels[depth].items[row];let(info,label)=read_raw(self.levels[depth].menu,item.position)?;
        if !same_identity(item,&info,&label){return self.rebuild_level(depth,gfx,colors);}
        if info.fState.0&(MFS_DISABLED|MFS_GRAYED).0!=0||!item.selectable(){return self.refresh_states(gfx,colors);}
        if item.cascade(){self.open_child(depth,row,gfx,colors,keyboard)}else{Ok(Step::Choose(item.id))}
    }
}
impl Drop for Session{fn drop(&mut self){let root=ROOT.with(Cell::get);if !root.0.is_null(){let _=unsafe{KillTimer(Some(root),TIMER)};}
    ROOT.with(|value|value.set(HWND::default()));NATIVE_TRACKING.with(|value|value.set(true));
    if self.levels.iter().any(|level|unsafe{GetCapture()}==level.hwnd){let _=unsafe{ReleaseCapture()};}
    self.tooltip.take();self.close_from(0);NATIVE_TRACKING.with(|value|value.set(false));
    self.provider.message(WM_MENUSELECT,WPARAM(0xffff0000),LPARAM(0));
    if self.entered&&unsafe{IsWindow(Some(self.owner))}.as_bool(){unsafe{SendMessageW(self.owner,WM_EXITMENULOOP,Some(WPARAM(1)),Some(LPARAM(0)));}}
    let _=unsafe{PostMessageW(Some(self.owner),WM_NULL,WPARAM(0),LPARAM(0))};}}
enum Step{Continue,Choose(u32),Dismiss}
fn hit(levels:&[Level],point:POINT)->Option<(usize,Option<usize>)>{levels.iter().enumerate().rev().find(|(_,level)|level.body.contains(point.x,point.y)).map(|(depth,level)|(depth,level.row_at(point)))}

fn in_triangle(point:POINT,a:POINT,b:POINT,c:POINT)->bool{
    let cross=|a:POINT,b:POINT,p:POINT|(b.x-a.x)as i64*(p.y-a.y)as i64-(b.y-a.y)as i64*(p.x-a.x)as i64;
    let values=[cross(a,b,point),cross(b,c,point),cross(c,a,point)];!(values.iter().any(|v|*v<0)&&values.iter().any(|v|*v>0))
}
/// A diagonal move across a neighbouring parent row is still aiming at the
/// already-open child. Keep that path alive briefly instead of switching it.
fn aiming_at_child(levels:&[Level],depth:usize,previous:POINT,current:POINT)->bool{
    let Some(child)=levels.get(depth+1)else{return false;};let parent=&levels[depth];
    let right=child.body.left>=parent.body.left;let edge=if right{child.body.left}else{child.body.right};
    if right&&current.x<=previous.x||!right&&current.x>=previous.x{return false;}
    in_triangle(current,previous,POINT{x:edge,y:child.body.top-8},POINT{x:edge,y:child.body.bottom+8})
}
fn message_point(message:&MSG)->POINT{
    if matches!(message.message,WM_MOUSEWHEEL|WM_MOUSEHWHEEL){return POINT{x:message.lParam.0 as i16 as i32,y:(message.lParam.0>>16)as i16 as i32};}
    let mut point=POINT{x:message.lParam.0 as i16 as i32,y:(message.lParam.0>>16)as i16 as i32};
    if unsafe{ClientToScreen(message.hwnd,&mut point)}.as_bool(){point}else{message.pt}
}
fn key(session:&mut Session,vk:VIRTUAL_KEY,gfx:&Gfx,colors:&Palette)->Result<Step>{
    let depth=session.levels.len()-1;
    match vk{
        VK_DOWN|VK_UP|VK_HOME|VK_END|VK_PRIOR|VK_NEXT=>{
            let level=&mut session.levels[depth];let order=level.visual_order();let body=level.body_order();
            let next=if matches!(vk,VK_PRIOR|VK_NEXT){let direction=if vk==VK_NEXT{1}else{-1};let page=(level.body.height()as f32/level.metrics.px(geometry::ITEM_HEIGHT_DIP)).max(1.)as usize;
                let mut next=level.highlight;for _ in 0..page{next=next_in_order(&body,next,direction);}next}
                else if matches!(vk,VK_HOME|VK_END){next_in_order(&order,None,if vk==VK_HOME{1}else{-1})}
                else if level.highlight.is_some_and(|row|level.toolbar.contains(&row)){next_in_order(&body,None,if vk==VK_DOWN{1}else{-1}).or(level.highlight)}
                else if vk==VK_UP&&level.highlight==body.first().copied()&&!level.toolbar.is_empty(){level.toolbar.iter().rev().copied().find(|row|level.items[*row].selectable()).or_else(||next_in_order(&body,level.highlight,-1))}
                else if vk==VK_DOWN&&level.highlight.is_some()&&level.highlight==body.last().copied()&&!level.toolbar.is_empty(){level.toolbar.iter().copied().find(|row|level.items[*row].selectable()).or_else(||next_in_order(&body,level.highlight,1))}
                else{next_in_order(&body,level.highlight,if vk==VK_DOWN{1}else{-1}).or_else(||next_in_order(&order,None,1))};
            if let Some(row)=next{level.ensure_visible(row);}level.highlight=None;session.highlight(depth,next,false,gfx,colors)?;Ok(Step::Continue)
        },
        VK_LEFT|VK_RIGHT if session.levels[depth].highlight.is_some_and(|row|session.levels[depth].toolbar.contains(&row))=>{
            let level=&session.levels[depth];let order:Vec<_>=level.toolbar.iter().copied().filter(|row|level.items[*row].selectable()).collect();
            let next=next_in_order(&order,level.highlight,if vk==VK_RIGHT{1}else{-1});session.highlight(depth,next,false,gfx,colors)?;Ok(Step::Continue)
        },
        VK_RIGHT=>{if let Some(row)=session.levels[depth].highlight{if session.levels[depth].items[row].cascade(){return session.activate(depth,row,gfx,colors,true);}}Ok(Step::Continue)},
        VK_LEFT|VK_ESCAPE=>{if depth==0{return Ok(if vk==VK_ESCAPE{Step::Dismiss}else{Step::Continue});}session.close_from(depth);session.present(depth-1,gfx,colors)?;Ok(Step::Continue)},
        VK_RETURN|VK_SPACE=>{if let Some(row)=session.levels[depth].highlight{session.activate(depth,row,gfx,colors,true)}else{Ok(Step::Continue)}},
        VK_MENU|VK_F10|VK_APPS|VK_LWIN|VK_RWIN|VK_TAB=>Ok(Step::Dismiss),_=>Ok(Step::Continue),
    }
}
fn mnemonic(session:&mut Session,character:char,gfx:&Gfx,colors:&Palette)->Result<Step>{
    let depth=session.levels.len()-1;let wanted=character.to_lowercase().next().unwrap_or(character);let level=&session.levels[depth];
    let explicit:Vec<_>=level.items.iter().enumerate().filter(|(_,item)|item.selectable()&&item.mnemonic==Some(wanted)).map(|(index,_)|index).collect();
    let choices=if explicit.is_empty(){level.items.iter().enumerate().filter(|(_,item)|item.selectable()&&item.label.chars().next().and_then(|c|c.to_lowercase().next())==Some(wanted)).map(|(index,_)|index).collect()}else{explicit};
    if choices.is_empty(){
        if let Some(value)=session.provider.message(WM_MENUCHAR,WPARAM((MF_POPUP.0 as usize)<<16|character as usize&0xffff),LPARAM(level.menu.0 as isize)){
            let action=(value.0 as usize>>16)&0xffff;let row=value.0 as usize&0xffff;
            if action==1{return key(session,VK_ESCAPE,gfx,colors);}
            if row<session.levels[depth].items.len()&&session.levels[depth].items[row].selectable(){
                if action==2{return session.activate(depth,row,gfx,colors,true);}
                if action==3{session.levels[depth].ensure_visible(row);session.highlight(depth,Some(row),false,gfx,colors)?;}
            }
        }return Ok(Step::Continue);
    }
    let row=if choices.len()==1{choices[0]}else{choices.iter().copied().find(|row|Some(*row)>session.levels[depth].highlight).unwrap_or(choices[0])};
    session.levels[depth].ensure_visible(row);session.highlight(depth,Some(row),false,gfx,colors)?;
    if choices.len()==1{session.activate(depth,row,gfx,colors,true)}else{Ok(Step::Continue)}
}

pub(super) fn track(owner:HWND,menu:HMENU,point:POINT,context:&IContextMenu,options:&ContextMenuOptions,filter:bool)->Result<Option<u32>>{
    track_initialized(owner,menu,point,context,options,filter,false)
}
/// `root_initialized` transfers one existing popup initialization lease, as
/// needed when the caller has extracted and initialized ShellNew's submenu.
pub(super) fn track_initialized(owner:HWND,menu:HMENU,point:POINT,context:&IContextMenu,options:&ContextMenuOptions,filter:bool,root_initialized:bool)->Result<Option<u32>>{
    if is_open(){return Err(error(0x800700aa,"已有菜单正在处理"));}
    if menu.is_invalid()||!unsafe{IsWindow(Some(owner))}.as_bool(){return Err(error(0x80070057,"菜单或所属窗口已失效"));}
    ACTIVE.with(|value|value.set(true));CANCEL.with(|value|value.set(false));
    struct Reset;impl Drop for Reset{fn drop(&mut self){ACTIVE.with(|value|value.set(false));CANCEL.with(|value|value.set(false));ROOT.with(|value|value.set(HWND::default()));}}
    let _reset=Reset;
    let provider=Provider{context:context.clone(),options:options.clone(),root:menu,filter,new_root:root_initialized&&!filter};
    if crate::theme::high_contrast(){let result=native_fallback(&provider,owner,menu,point,root_initialized);if root_initialized{provider.uninitialize(menu);}return result;}
    let gfx=match register_class().and_then(|_|graphics()){Ok(gfx)=>gfx,Err(error)=>{eprintln!("drawn-menu graphics unavailable: {error}");let result=native_fallback(&provider,owner,menu,point,root_initialized);if root_initialized{provider.uninitialize(menu);}return result;}};
    let colors=palette(crate::theme::menus_dark());
    provider.message(WM_INITMENU,WPARAM(menu.0 as usize),LPARAM(0));
    let Some(root)=open_level(&provider,&gfx,owner,menu,0,Place::Root(point),root_initialized)?else{return Ok(None);};
    let mut session=Session{provider,owner,levels:vec![root],entered:false,revision:0,tooltip:None,tip_target:None,tip_shown:false};let root=session.levels[0].hwnd;ROOT.with(|value|value.set(root));
    if CANCEL.with(Cell::get)||!unsafe{IsWindow(Some(owner))}.as_bool(){return Ok(None);}
    if session.levels[0].native_only{return native_fallback(&session.provider,owner,menu,point,true);}
    session.present(0,&gfx,&colors)?;session.entered=true;
    unsafe{SendMessageW(owner,WM_ENTERMENULOOP,Some(WPARAM(1)),Some(LPARAM(0)));SetCapture(root);}
    if unsafe{SetTimer(Some(root),TIMER,30,None)}==0{return Err(Error::from_thread());}
    let foreground=unsafe{GetForegroundWindow()};let mut start=POINT::default();let _=unsafe{GetCursorPos(&mut start)};
    let mut previous=start;let mut armed=false;let mut pending:Option<(usize,usize,Instant,Duration)>=None;
    let mut selected=None;let mut refreshed=Instant::now();
    loop{
        if CANCEL.with(Cell::get){break;}
        let mut message=MSG::default();let result=unsafe{GetMessageW(&mut message,None,0,0)}.0;
        if result==0{unsafe{PostQuitMessage(message.wParam.0 as i32);}break;}if result<0{return Err(Error::from_thread());}
        let pointer=message_point(&message);
        let step=match message.message{
            WM_MOUSEMOVE=>{
                if (pointer.x-start.x).abs()>2||(pointer.y-start.y).abs()>2{armed=true;}
                if let Some((depth,row))=hit(&session.levels,pointer){
                    if pending.is_some_and(|(at,_,_,_)|at<depth){pending=None;}
                    let changed=session.levels[depth].highlight!=row;
                    if changed&&session.levels[depth].open_child.is_some()&&aiming_at_child(&session.levels,depth,previous,pointer){
                        if let Some(row)=row{if !pending.is_some_and(|(at,old,_,delay)|at==depth&&old==row&&delay==AIM_DELAY){pending=Some((depth,row,Instant::now(),AIM_DELAY));}}
                    }else{
                        let before=session.levels[depth].highlight;session.highlight(depth,row,true,&gfx,&colors)?;
                        if session.levels[depth].highlight!=before{pending=session.levels[depth].highlight.map(|row|(depth,row,Instant::now(),HOVER_DELAY));}
                    }
                }else{pending=None;}
                let tip=hit(&session.levels,pointer).and_then(|(depth,row)|row.filter(|row|session.levels[depth].toolbar.contains(row)).map(|row|(depth,row)));
                session.schedule_tooltip(tip);previous=pointer;Step::Continue
            },
            WM_LBUTTONDOWN|WM_RBUTTONDOWN|WM_MBUTTONDOWN|WM_XBUTTONDOWN|WM_LBUTTONDBLCLK|WM_RBUTTONDBLCLK=>{
                armed=true;pending=None;session.hide_tooltip();
                match hit(&session.levels,pointer){None=>Step::Dismiss,Some((depth,Some(row))) if matches!(message.message,WM_LBUTTONDOWN|WM_RBUTTONDOWN|WM_LBUTTONDBLCLK|WM_RBUTTONDBLCLK)=>{
                    session.highlight(depth,Some(row),true,&gfx,&colors)?;if session.levels[depth].highlight==Some(row){session.levels[depth].pressed=Some(row);session.present(depth,&gfx,&colors)?;
                        if session.levels[depth].items[row].cascade(){session.activate(depth,row,&gfx,&colors,false)?}else{Step::Continue}}else{Step::Continue}},_=>Step::Continue}
            },
            WM_LBUTTONUP|WM_RBUTTONUP=>match hit(&session.levels,pointer){Some((depth,Some(row))) if armed&&session.levels[depth].pressed==Some(row)&&!session.levels[depth].items[row].cascade()=>session.activate(depth,row,&gfx,&colors,false)?,
                Some((depth,_))=>{session.levels[depth].pressed=None;session.present(depth,&gfx,&colors)?;Step::Continue},_=>Step::Continue},
            WM_MOUSEWHEEL=>{pending=None;session.hide_tooltip();let depth=hit(&session.levels,pointer).map_or(session.levels.len()-1,|(depth,_)|depth);
                let delta=((message.wParam.0>>16)as i16)as f32/120.;let level=&mut session.levels[depth];let scroll=(level.scroll-delta*level.metrics.px(geometry::ITEM_HEIGHT_DIP)*3.).clamp(0.,level.max_scroll());
                if scroll!=level.scroll{level.scroll=scroll;session.close_from(depth+1);let row=session.levels[depth].row_at(pointer);session.highlight(depth,row,true,&gfx,&colors)?;session.present(depth,&gfx,&colors)?;}Step::Continue},
            WM_KEYDOWN|WM_SYSKEYDOWN=>{pending=None;session.hide_tooltip();let vk=VIRTUAL_KEY(message.wParam.0 as u16);
                if [VK_UP,VK_DOWN,VK_HOME,VK_END,VK_PRIOR,VK_NEXT,VK_LEFT,VK_RIGHT,VK_RETURN,VK_SPACE,VK_ESCAPE,VK_MENU,VK_F10,VK_APPS,VK_LWIN,VK_RWIN,VK_TAB].contains(&vk){key(&mut session,vk,&gfx,&colors)?}
                else{let _=unsafe{TranslateMessage(&message)};Step::Continue}},
            WM_CHAR|WM_SYSCHAR=>if let Some(character)=char::from_u32(message.wParam.0 as u32).filter(|c|!c.is_control()){mnemonic(&mut session,character,&gfx,&colors)?}else{Step::Continue},
            WM_KEYUP|WM_SYSKEYUP|WM_DEADCHAR|WM_SYSDEADCHAR=>Step::Continue,
            WM_TIMER if message.hwnd==root&&message.wParam.0==TIMER=>{
                if !unsafe{IsWindow(Some(owner))}.as_bool()||unsafe{GetForegroundWindow()}!=foreground{Step::Dismiss}
                else{
                    session.update_tooltip();
                    let revision=session.revision;
                    let update=if refreshed.elapsed()>=Duration::from_millis(120){refreshed=Instant::now();session.refresh_states(&gfx,&colors)?}else{Step::Continue};
                    if revision!=session.revision{pending=hit(&session.levels,previous).and_then(|(depth,row)|row.filter(|row|session.levels[depth].items[*row].selectable()).map(|row|(depth,row,Instant::now(),HOVER_DELAY)));}
                    if !matches!(update,Step::Continue){update}
                    else if let Some((depth,row,since,delay))=pending{
                    if since.elapsed()>=delay{pending=None;if depth<session.levels.len(){
                        // Use delivered pointer coordinates. A global cursor
                        // query can race queued input or another desktop.
                        if hit(&session.levels,previous).is_some_and(|(at,hit)|at==depth&&hit==Some(row)){
                            session.highlight(depth,Some(row),true,&gfx,&colors)?;session.close_from(depth+1);
                            if session.levels[depth].items[row].cascade(){session.activate(depth,row,&gfx,&colors,false)?}else{session.present(depth,&gfx,&colors)?;Step::Continue}
                        }else{Step::Continue}
                    }else{Step::Continue}}else{Step::Continue}
                    }else{Step::Continue}
                }
            },
            WM_DISPLAYCHANGE|WM_DPICHANGED=>Step::Dismiss,
            WM_THEMECHANGED|WM_SYSCOLORCHANGE=>{GRAPHICS.with(|slot|{slot.borrow_mut().take();});Step::Dismiss},
            _=>{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}Step::Continue},
        };
        match step{Step::Continue=>(),Step::Choose(id)=>{selected=Some(id);break;},Step::Dismiss=>break,}
    }
    // Pending menu characters must not become text in the underlying file view.
    let mut stale=MSG::default();while unsafe{PeekMessageW(&mut stale,None,WM_CHAR,WM_DEADCHAR,PM_REMOVE)}.as_bool(){}
    while unsafe{PeekMessageW(&mut stale,None,WM_SYSCHAR,WM_SYSDEADCHAR,PM_REMOVE)}.as_bool(){}
    Ok(selected)
}

#[cfg(test)]mod tests{
    use super::*;
    use std::{path::PathBuf,thread};
    use anyhow::{Context,ensure};
    use windows::{core::{implement,PCWSTR,PSTR},Win32::UI::Shell::{IContextMenu_Impl,IContextMenu2_Impl,IContextMenu3_Impl,CMINVOKECOMMANDINFO}};
    thread_local!{static DRIVER:Cell<(u32,u32)>=const{Cell::new((0,0))};}
    const DRIVER_TIMER:usize=0x5a445254;
    const WATCHDOG_TIMER:usize=0x5a445257;
    #[link(name="user32")]unsafe extern "system"{
        fn CreateDesktopW(name:*const u16,device:*const u16,mode:*const core::ffi::c_void,flags:u32,access:u32,security:*const core::ffi::c_void)->*mut core::ffi::c_void;
        fn SetThreadDesktop(desktop:*mut core::ffi::c_void)->i32;
        fn CloseDesktop(desktop:*mut core::ffi::c_void)->i32;
    }
    #[derive(Default)]struct Counts{initialize:Cell<u32>,uninitialize:Cell<u32>,draw:Cell<u32>,measure:Cell<u32>,invoke:Cell<u32>}
    #[implement(IContextMenu3)]struct DynamicMenu{child:HMENU,counts:Rc<Counts>,toolbar:bool}
    impl DynamicMenu{
        fn notify(&self,message:u32,wp:WPARAM,lp:LPARAM)->Result<LRESULT>{
            if message==WM_INITMENUPOPUP&&HMENU(wp.0 as*mut _)==self.child{
                self.counts.initialize.set(self.counts.initialize.get()+1);
                while unsafe{GetMenuItemCount(Some(self.child))}>0{unsafe{DeleteMenu(self.child,0,MF_BYPOSITION)}?;}
                unsafe{AppendMenuW(self.child,MF_STRING,10,w!("文件夹(&F)"))?;AppendMenuW(self.child,MF_STRING|MF_CHECKED,11,w!("文档(&D)\tCtrl+N"))?;
                    AppendMenuW(self.child,MF_STRING|MF_GRAYED,12,w!("不可用模板"))?;SetMenuDefaultItem(self.child,0,1)?;}
            }else if message==WM_UNINITMENUPOPUP&&HMENU(wp.0 as*mut _)==self.child{self.counts.uninitialize.set(self.counts.uninitialize.get()+1);}
            else if message==WM_MEASUREITEM{let value=unsafe{&mut*(lp.0 as*mut MEASUREITEMSTRUCT)};if value.itemID==50{self.counts.measure.set(self.counts.measure.get()+1);value.itemWidth=240;value.itemHeight=36;}}
            else if message==WM_DRAWITEM{let value=unsafe{&*(lp.0 as*const DRAWITEMSTRUCT)};if value.itemID==50{assert_eq!(value.itemData,0x5a5344);self.counts.draw.set(self.counts.draw.get()+1);
                let brush=unsafe{CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x00664022))};unsafe{FillRect(value.hDC,&value.rcItem,brush);let _=DeleteObject(brush.into());}}}
            Ok(LRESULT(0))
        }
    }
    #[allow(non_snake_case)]impl IContextMenu_Impl for DynamicMenu_Impl{
        fn QueryContextMenu(&self,menu:HMENU,_:u32,_:u32,_:u32,_:u32)->HRESULT{
            let result=(||->Result<()>{unsafe{
                if self.toolbar{AppendMenuW(menu,MF_STRING,1,w!("剪切(&T)\tCtrl+X"))?;AppendMenuW(menu,MF_STRING|MF_GRAYED,2,w!("复制(&C)\tCtrl+C"))?;
                    AppendMenuW(menu,MF_STRING,3,w!("重命名(&M)\tF2"))?;AppendMenuW(menu,MF_STRING,4,w!("共享"))?;AppendMenuW(menu,MF_STRING,5,w!("删除(&D)\tDelete"))?;}
                AppendMenuW(menu,MF_POPUP,self.child.0 as usize,w!("新建(&W)"))?;AppendMenuW(menu,MF_SEPARATOR,0,None)?;
                if !self.toolbar{
                AppendMenuW(menu,MF_STRING|MF_GRAYED,2,w!("复制(&C)"))?;AppendMenuW(menu,MF_OWNERDRAW,50,PCWSTR(0x5a5344usize as*const u16))?;}}Ok(())})();
            result.map_or_else(|error|error.code(),|_|HRESULT(51))
        }
        fn InvokeCommand(&self,_:*const CMINVOKECOMMANDINFO)->Result<()>{self.counts.invoke.set(self.counts.invoke.get()+1);Ok(())}
        fn GetCommandString(&self,id:usize,flags:u32,_:*const u32,buffer:PSTR,capacity:u32)->Result<()>{
            if self.toolbar&&flags==windows::Win32::UI::Shell::GCS_VERBW&&id<5{
                let verb=["cut","copy","rename","Windows.ModernShare","delete"][id];let wide:Vec<u16>=verb.encode_utf16().chain(Some(0)).collect();
                if capacity as usize>=wide.len()&&!buffer.is_null(){unsafe{std::ptr::copy_nonoverlapping(wide.as_ptr(),buffer.0.cast::<u16>(),wide.len());}return Ok(());}}
            Err(Error::from_hresult(HRESULT(0x80004001u32 as i32)))
        }
    }
    #[allow(non_snake_case)]impl IContextMenu2_Impl for DynamicMenu_Impl{fn HandleMenuMsg(&self,message:u32,wp:WPARAM,lp:LPARAM)->Result<()>{self.notify(message,wp,lp).map(|_|())}}
    #[allow(non_snake_case)]impl IContextMenu3_Impl for DynamicMenu_Impl{fn HandleMenuMsg2(&self,message:u32,wp:WPARAM,lp:LPARAM,result:*mut LRESULT)->Result<()>{
        let value=self.notify(message,wp,lp)?;if !result.is_null(){unsafe{*result=value;}}Ok(())}}
    fn post_key(hwnd:HWND,vk:VIRTUAL_KEY){let _=unsafe{PostMessageW(Some(hwnd),WM_KEYDOWN,WPARAM(vk.0 as usize),LPARAM(0))};}
    fn post_pointer(root:HWND,message:u32,point:POINT){let mut local=point;let _=unsafe{ScreenToClient(root,&mut local)};
        let packed=((local.x as u16 as u32)|((local.y as u16 as u32)<<16))as isize;let _=unsafe{PostMessageW(Some(root),message,WPARAM(0),LPARAM(packed))};}
    fn row_point(snapshot:&serde_json::Value,depth:usize,row:usize)->POINT{
        let level=&snapshot["levels"][depth];let bounds=level["bounds"].as_array().unwrap();let item=&level["rows"][row];
        POINT{x:bounds[0].as_i64().unwrap()as i32+40,y:bounds[1].as_i64().unwrap()as i32+(item["y"].as_f64().unwrap()+item["height"].as_f64().unwrap()/2.)as i32}
    }
    unsafe extern "system" fn driver(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
        if message==WM_ENTERMENULOOP{
            if let Some(root)=popup_window(){let(mode,_)=DRIVER.with(Cell::get);
                if mode==1{for key in [VK_HOME,VK_RIGHT,VK_DOWN,VK_RETURN]{post_key(root,key);}}
                else if mode==3{for key in [VK_HOME,VK_RETURN]{post_key(root,key);}}
                else if mode==5{for key in [VK_HOME,VK_RIGHT,VK_RIGHT,VK_LEFT,VK_RETURN]{post_key(root,key);}}
                else if mode==6{for key in [VK_HOME,VK_DOWN,VK_RIGHT,VK_RETURN]{post_key(root,key);}}
                else{if mode==4{for key in [VK_HOME,VK_RIGHT]{post_key(root,key);}}unsafe{SetTimer(Some(hwnd),DRIVER_TIMER,40,None);}}
            }return LRESULT(0);
        }
        if message==WM_TIMER&&wp.0==WATCHDOG_TIMER{cancel();return LRESULT(0);}
        if message==WM_TIMER&&wp.0==DRIVER_TIMER{
            let Some(root)=popup_window()else{return LRESULT(0);};let snapshot=diagnostic_snapshot();let count=snapshot["levels"].as_array().map_or(0,Vec::len);
            let(mode,step)=DRIVER.with(Cell::get);
            if mode==4{
                if step==0&&count==2{
                    let child=HMENU(snapshot["levels"][1]["menu"].as_u64().unwrap()as usize as*mut _);
                    let mut label:Vec<u16>="变更后的文档(&D)".encode_utf16().chain(Some(0)).collect();
                    let info=MENUITEMINFOW{cbSize:std::mem::size_of::<MENUITEMINFOW>()as u32,fMask:MIIM_ID|MIIM_STRING,wID:13,dwTypeData:PWSTR(label.as_mut_ptr()),..Default::default()};
                    let _=unsafe{SetMenuItemInfoW(child,1,true,&info)};
                    for key in [VK_DOWN,VK_RETURN]{post_key(root,key);}DRIVER.with(|value|value.set((mode,1)));
                }else if step==1&&count==2&&snapshot["levels"][1]["rows"][1]["id"]==13{
                    for key in [VK_HOME,VK_DOWN,VK_RETURN]{post_key(root,key);}DRIVER.with(|value|value.set((mode,2)));
                }
                return LRESULT(0);
            }
            match step{
                0 if count==1=>{post_pointer(root,WM_MOUSEMOVE,row_point(&snapshot,0,0));DRIVER.with(|value|value.set((mode,1)));},
                1 if count==2=>{let parent=&snapshot["levels"][0];let bounds=parent["bounds"].as_array().unwrap();let mut point=row_point(&snapshot,0,3);point.x=bounds[2].as_i64().unwrap()as i32-8;
                    post_pointer(root,WM_MOUSEMOVE,point);DRIVER.with(|value|value.set((mode,2)));},
                2..=8 if count==2=>{DRIVER.with(|value|value.set((mode,step+1)));},
                9 if count==2=>{post_pointer(root,WM_MOUSEMOVE,row_point(&snapshot,1,1));DRIVER.with(|value|value.set((mode,10)));},
                10 if count==2=>{let point=row_point(&snapshot,1,1);post_pointer(root,WM_LBUTTONDOWN,point);post_pointer(root,WM_LBUTTONUP,point);DRIVER.with(|value|value.set((mode,11)));},
                _=>(),
            }return LRESULT(0);
        }
        if message==WM_EXITMENULOOP{let _=unsafe{KillTimer(Some(hwnd),DRIVER_TIMER)};return LRESULT(0);}
        unsafe{DefWindowProcW(hwnd,message,wp,lp)}
    }
    fn exercise()->anyhow::Result<serde_json::Value>{
        unsafe{windows::Win32::System::Ole::OleInitialize(None)}?;
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){GRAPHICS.with(|value|{value.borrow_mut().take();});unsafe{windows::Win32::System::Ole::OleUninitialize();}}}let _apartment=Apartment;
        ensure!(!crate::theme::high_contrast(),"drawn-style test requires standard contrast; native fallback is a separate behavior");
        let module=unsafe{GetModuleHandleW(None)}?;let class=w!("ZSDesk.DrawnShellMenu.Test");unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(driver),hInstance:module.into(),lpszClassName:class,..Default::default()});}
        let hwnd=unsafe{CreateWindowExW(WS_EX_NOACTIVATE|WS_EX_TOOLWINDOW,class,w!("Shell menu fixture"),WS_POPUP,20,20,400,300,None,None,Some(module.into()),None)}?;
        struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}let _window=Window(hwnd);
        unsafe{let _=ShowWindow(hwnd,SW_SHOWNOACTIVATE);let _=SetForegroundWindow(hwnd);SetTimer(Some(hwnd),WATCHDOG_TIMER,5000,None);}
        let menu=unsafe{CreatePopupMenu()}?;struct Menu(HMENU);impl Drop for Menu{fn drop(&mut self){let _=unsafe{DestroyMenu(self.0)};}}let _menu=Menu(menu);
        let child=unsafe{CreatePopupMenu()}?;let counts=Rc::new(Counts::default());let menu3:IContextMenu3=DynamicMenu{child,counts:counts.clone(),toolbar:false}.into();let context:IContextMenu=menu3.cast()?;
        unsafe{context.QueryContextMenu(menu,0,1,0x6fff,0)}.ok()?;
        let options=ContextMenuOptions::default();let provider=Provider{context:context.clone(),options:options.clone(),root:menu,filter:false,new_root:false};
        let gfx=graphics()?;let(_,dpi)=monitor_at(POINT{x:70,y:70});let(items,_)=read_items(&provider,menu,&gfx,Metrics::new(dpi))?;
        ensure!(items.len()==4&&items[0].cascade()&&!items[2].enabled&&items[3].owner_draw,"native rows/state were not retained");
        ensure!(counts.initialize.get()==0,"inspection eagerly opened the New submenu");
        DRIVER.with(|value|value.set((1,0)));let selected=track(hwnd,menu,POINT{x:70,y:70},&context,&options,false)?;
        ensure!(selected==Some(11)&&counts.initialize.get()==1&&counts.uninitialize.get()==1,"keyboard lazy cascade lifecycle or ID failed");
        ensure!(!is_open()&&popup_window().is_none()&&counts.invoke.get()==0,"tracking executed a command or leaked popup state");
        DRIVER.with(|value|value.set((2,0)));let selected=track(hwnd,menu,POINT{x:70,y:70},&context,&options,false)?;
        ensure!(selected==Some(11)&&counts.initialize.get()==2&&counts.uninitialize.get()==2,"hover path reopened/closed New while moving into it");
        provider.initialize(child,0);DRIVER.with(|value|value.set((3,0)));
        let selected=track_initialized(hwnd,child,POINT{x:70,y:70},&context,&options,false,true)?;
        ensure!(selected==Some(10)&&counts.initialize.get()==3&&counts.uninitialize.get()==3,"preinitialized New root was initialized twice");
        DRIVER.with(|value|value.set((4,0)));let selected=track(hwnd,menu,POINT{x:70,y:70},&context,&options,false)?;
        ensure!(selected==Some(13)&&counts.initialize.get()==4&&counts.uninitialize.get()==4,"a replaced menu row executed a stale ID or reinitialized its submenu");
        ensure!(counts.draw.get()>0&&counts.measure.get()>0,"owner-drawn provider callbacks were lost");
        ensure!(unsafe{GetMenuItemCount(Some(child))}==3,"lazy New entries accumulated duplicates");
        let actions_menu=unsafe{CreatePopupMenu()}?;let _actions_menu=Menu(actions_menu);let actions_child=unsafe{CreatePopupMenu()}?;
        let actions3:IContextMenu3=DynamicMenu{child:actions_child,counts:counts.clone(),toolbar:true}.into();let actions:IContextMenu=actions3.cast()?;
        unsafe{actions.QueryContextMenu(actions_menu,0,1,0x6fff,0)}.ok()?;
        let actions_provider=Provider{context:actions.clone(),options:options.clone(),root:actions_menu,filter:false,new_root:false};
        let(items,_)=read_items(&actions_provider,actions_menu,&gfx,Metrics::new(dpi))?;
        let(layout,toolbar,_)=measure_items(&items,&gfx,Metrics::new(dpi),false);
        ensure!(toolbar==vec![0,1,2,3,4]&&!items[1].enabled&&layout.rows[5].h>0.,"toolbar changed native row positions or lost disabled/body commands");
        DRIVER.with(|value|value.set((5,0)));let selected=track(hwnd,actions_menu,POINT{x:70,y:70},&actions,&options,false)?;
        ensure!(selected==Some(3),"toolbar left/right did not skip disabled native command or retain rename ID");
        DRIVER.with(|value|value.set((6,0)));let selected=track(hwnd,actions_menu,POINT{x:70,y:70},&actions,&options,false)?;
        ensure!(selected==Some(10)&&counts.initialize.get()==5&&counts.uninitialize.get()==5,"toolbar-to-body keyboard path broke lazy New submenu");
        let _=unsafe{KillTimer(Some(hwnd),WATCHDOG_TIMER)};
        Ok(serde_json::json!({"keyboard_native_id":true,"lazy_new_submenu":true,"new_initialized_once_per_open":true,"preinitialized_new_root_not_reinitialized":true,
            "hover_travel_preserves_submenu":true,"native_disabled_state":true,"owner_draw_measure_and_paint":true,"stale_item_identity_rejected":true,"dynamic_rows_updated_without_reinitializing":true,"provider_invocations":counts.invoke.get(),"native_menus_retained":true,
            "toolbar_native_ids_and_disabled_skip":true,"toolbar_to_body_lazy_submenu":true,"initializations":counts.initialize.get(),"uninitializations":counts.uninitialize.get()}))
    }
    #[test]#[ignore="Owned private desktop; real drawn popup and fake dynamic ShellNew/owner-draw provider; requires ZSDESK_NATIVE_TEST_OUTPUT"]
    fn native_drawn_shell_lazy_submenus_and_owner_draw()->anyhow::Result<()>{
        let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("native output")?);ensure!(output.is_absolute(),"absolute output required");
        let name:Vec<u16>=format!("ZSDesk.DrawnShell.{}",std::process::id()).encode_utf16().chain(Some(0)).collect();
        let desktop=unsafe{CreateDesktopW(name.as_ptr(),std::ptr::null(),std::ptr::null(),0,0x000f00ff,std::ptr::null())};ensure!(!desktop.is_null(),"private desktop creation failed");
        let raw=desktop as usize;let result=thread::spawn(move||->anyhow::Result<serde_json::Value>{ensure!(unsafe{SetThreadDesktop(raw as*mut _)}!=0,"private desktop selection failed");exercise()}).join().map_err(|_|anyhow::anyhow!("drawn menu test thread panicked")).and_then(|result|result);
        let closed=unsafe{CloseDesktop(desktop)}!=0;let mut foreground=0;unsafe{GetWindowThreadProcessId(GetForegroundWindow(),Some(&mut foreground));}
        let isolated=foreground!=std::process::id();let report=serde_json::json!({"state":if result.is_ok()&&closed&&isolated{"passed"}else{"failed"},"checks":result.as_ref().map_err(|error|format!("{error:#}")),"desktop_closed":closed,"test_did_not_activate":isolated,"physical_input_generated":false});
        std::fs::write(output.join("drawn-shell-menu.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");result?;ensure!(closed&&isolated,"private popup cleanup/foreground failed");Ok(())
    }
    #[test]fn submenu_aim_corridor_keeps_diagonal_pointer_travel(){
        let a=POINT{x:100,y:140};let b=POINT{x:200,y:100};let c=POINT{x:200,y:350};
        assert!(in_triangle(POINT{x:150,y:180},a,b,c));assert!(!in_triangle(POINT{x:90,y:180},a,b,c));
        assert!(in_triangle(POINT{x:150,y:180},a,c,b));
    }
    #[test]fn sign_extended_gdi_handles_are_not_menu_bitmap_sentinels(){
        assert!(valid_bitmap(HBITMAP((-1593503439isize)as*mut _)));
        assert!(valid_bitmap(HBITMAP(0x210034usize as*mut _)));
        assert!(!valid_bitmap(HBITMAP::default()));
        assert!(!valid_bitmap(HBMMENU_CALLBACK));
        for value in 1..=11{assert!(!valid_bitmap(HBITMAP(value as*mut _)));}
    }
    #[test]fn localized_mnemonics_keep_keyboard_binding_without_classic_suffix(){
        let(label,key,shortcut)=shell_label("打开(&O)\tEnter");
        assert_eq!(label,"打开");assert_eq!(key,geometry::parse_label("打开(&O)").1);assert_eq!(shortcut,"Enter");
        assert_eq!(shell_label("Name (O)").0,"Name (O)");
        assert_eq!(shell_label("A && B").0,"A & B");
    }
    #[test]fn native_label_and_state_are_not_inferred_from_text(){
        let(label,mnemonic,accelerator)=geometry::parse_label("新建(&W)\tCtrl+N");assert_eq!(label,"新建(W)");assert_eq!(mnemonic,Some('w'));assert_eq!(accelerator,"Ctrl+N");
        assert!(!valid_bitmap(HBMMENU_CALLBACK));assert!(!valid_bitmap(HBITMAP::default()));
    }
    #[test]fn toolbar_promotes_only_unambiguous_native_actions(){
        let item=|id,action|Item{position:id,id,raw_label:"复制(&C)\tCtrl+C".into(),label:"复制(C)".into(),mnemonic:Some('c'),accelerator:"Ctrl+C".into(),kind:RowKind::Item,
            enabled:true,checked:false,radio:false,default:false,submenu:HMENU::default(),item_data:0,owner_draw:false,bitmap:HBITMAP::default(),measured:None,wic:None,image:None,action};
        let mut items=vec![item(41,Some(Action::Copy)),item(77,Some(Action::Copy)),item(80,Some(Action::Cut)),item(90,Some(Action::Delete))];
        items[3].owner_draw=true;
        assert_eq!(toolbar_items(&items),vec![2],"duplicate verbs and owner-draw rows must remain complete body entries");
        assert_eq!(action_label(&items[0]),"复制");assert_eq!(items[0].raw_label,"复制(&C)\tCtrl+C");
        assert_eq!(Action::from_verb("Windows.ModernShare"),Some(Action::Share));assert_eq!(Action::from_verb("sharewith"),None);
        assert_eq!(next_in_order(&[80,41,90],Some(80),1),Some(41));
        let work=Bounds::new(0,0,1920,1080);let parent=Bounds::new(100,100,400,600);
        assert_eq!(place_shell_submenu(parent,160,250,400,work,Metrics::new(96)),(392,156));
    }
}
