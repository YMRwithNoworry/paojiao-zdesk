//! Draw the common page without asking installed legacy handlers to build it.
//! Public Shell command IDs and dynamic submenus are materialized on demand.
use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::HashMap,
    mem::size_of,
    ptr,
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::SystemServices::{
            SFGAO_CANCOPY, SFGAO_CANDELETE, SFGAO_CANMOVE, SFGAO_CANRENAME, SFGAO_FILESYSTEM,
            SFGAO_FOLDER, SFGAO_LINK, SFGAO_STREAM,
        },
        UI::{
            Shell::{Common::*, *},
            WindowsAndMessaging::*,
        },
    },
    core::{Error, HRESULT, Interface, PCSTR, PSTR, PWSTR, Result, implement},
};

const SYSTEM_START: u32 = 64;
const RESERVED: u32 = SYSTEM_START + 512;
const NEW_COUNT: u32 = 512;
const FULL_OFFSET: u32 = RESERVED + NEW_COUNT;
const MORE: u32 = 31;
struct AppGroup {
    context: IContextMenu,
    first: u32,
    last: u32,
    root: HMENU,
}
#[derive(Clone, Copy)]
struct Row {
    offset: u32,
    verb: &'static str,
    label: &'static str,
    cascade: bool,
}
const fn item(offset: u32, verb: &'static str, label: &'static str) -> Row {
    Row {
        offset,
        verb,
        label,
        cascade: false,
    }
}
const fn group(offset: u32, verb: &'static str, label: &'static str) -> Row {
    Row {
        offset,
        verb,
        label,
        cascade: true,
    }
}
const COMMANDS: &[Row] = &[
    item(0, "cut", "剪切"),
    item(1, "copy", "复制"),
    item(2, "rename", "重命名"),
    item(3, "windows.modernshare", "共享"),
    item(4, "delete", "删除"),
    item(5, "open", "打开\tEnter"),
    group(6, "openas", "打开方式"),
    item(7, "copyaspath", "复制文件地址\tCtrl+Shift+C"),
    item(8, "properties", "属性\tAlt+Enter"),
    item(9, "pintostartscreen", "固定到“开始”"),
    item(10, "paste", "粘贴\tCtrl+V"),
    group(11, "view", "查看"),
    group(12, "arrange", "排序方式"),
    group(13, "groupby", "分组依据"),
    item(14, "refresh", "刷新"),
    group(15, "newfolder", "新建"),
    group(16, "sharewith", "共享对象"),
    item(17, "pintohomefile", "添加到收藏夹"),
    group(18, "compressto", "压缩到..."),
    item(19, "pintohome", "固定到快速访问"),
    item(20, "opennewtab", "在新标签页中打开"),
    item(21, "opennewwindow", "在新窗口中打开"),
    item(22, "Display", "显示设置"),
    item(23, "Personalize", "个性化"),
    item(24, "extract", "全部解压缩…"),
    item(25, "opencontaining", "打开文件所在的位置"),
    item(26, "runas", "以管理员身份运行"),
    item(27, "setdesktopwallpaper", "设置为桌面背景"),
    item(28, "rotate90", "向右旋转"),
    item(29, "rotate270", "向左旋转"),
    group(MORE, "zsdesk.more", "显示更多选项"),
];
fn err(text: &str) -> Error {
    Error::new(HRESULT(0x80004005u32 as i32), text)
}
fn info(menu: HMENU, index: u32) -> Result<(MENUITEMINFOW, Vec<u16>)> {
    let mut i = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_ID
            | MIIM_STRING
            | MIIM_FTYPE
            | MIIM_STATE
            | MIIM_SUBMENU
            | MIIM_DATA
            | MIIM_BITMAP
            | MIIM_CHECKMARKS,
        ..Default::default()
    };
    unsafe { GetMenuItemInfoW(menu, index, true, &mut i) }?;
    if i.cch > 8192 {
        return Err(err("菜单名称过长"));
    }
    let mut text = vec![0u16; i.cch as usize + 1];
    i.cch = text.len() as u32;
    i.dwTypeData = PWSTR(text.as_mut_ptr());
    unsafe { GetMenuItemInfoW(menu, index, true, &mut i) }?;
    Ok((i, text))
}
#[derive(Clone, Copy)]
struct Alias {
    source: HMENU,
    position: u32,
}
struct Templates {
    ctx: IContextMenu,
    menu: HMENU,
    child: HMENU,
}
impl Drop for Templates {
    fn drop(&mut self) {
        let _ = unsafe { DestroyMenu(self.menu) };
    }
}
#[implement(IContextMenu3, Agile = false)]
struct Compact {
    source: Source,
    background: bool,
    context: OnceCell<IContextMenu>,
    full: Cell<HMENU>,
    first: Cell<u32>,
    last: Cell<u32>,
    flags: Cell<u32>,
    loaded: Cell<bool>,
    roots: RefCell<HashMap<usize, Row>>,
    aliases: RefCell<HashMap<usize, Alias>>,
    groups: RefCell<Vec<std::rc::Rc<AppGroup>>>,
    group_menus: RefCell<HashMap<usize, usize>>,
    full_last: Cell<u32>,
    templates: OnceCell<Templates>,
    icons: RefCell<Vec<std::rc::Rc<super::packaged_overlay::Bitmap>>>,
    sys_nodes: RefCell<Vec<IExplorerCommand>>,
    sys_groups: RefCell<HashMap<usize, IExplorerCommand>>,
    sys_ready: RefCell<std::collections::HashSet<usize>>,
}
enum Source {
    View(IShellView),
    Selection {
        items: IShellItemArray,
        owner: HWND,
        view: OnceCell<IShellView>,
        resolve: Box<dyn Fn() -> Result<IShellView>>,
    },
}
pub(super) fn create(view: &IShellView, background: bool) -> IContextMenu {
    make(Source::View(view.clone()), background)
}
pub(super) fn for_selection(
    items: IShellItemArray,
    owner: HWND,
    resolve: Box<dyn Fn() -> Result<IShellView>>,
) -> IContextMenu {
    make(
        Source::Selection {
            items,
            owner,
            view: OnceCell::new(),
            resolve,
        },
        false,
    )
}
fn make(source: Source, background: bool) -> IContextMenu {
    let obj: IContextMenu3 = Compact {
        source,
        background,
        context: OnceCell::new(),
        full: Cell::new(HMENU::default()),
        first: Cell::new(0),
        last: Cell::new(0),
        flags: Cell::new(0),
        loaded: Cell::new(false),
        roots: Default::default(),
        aliases: Default::default(),
        groups: Default::default(),
        group_menus: Default::default(),
        full_last: Cell::new(0),
        templates: OnceCell::new(),
        sys_nodes: Default::default(),
        sys_groups: Default::default(),
        sys_ready: Default::default(),
        icons: Default::default(),
    }
    .into();
    obj.cast().expect("IContextMenu3 inherits IContextMenu")
}
impl Compact {
    fn view(&self) -> Result<&IShellView> {
        match &self.source {
            Source::View(view) => Ok(view),
            Source::Selection { view, resolve, .. } => {
                if view.get().is_none() {
                    let resolved = resolve()?;
                    let _ = view.set(resolved);
                }
                view.get().ok_or_else(|| err("菜单源视图尚未就绪"))
            }
        }
    }
    fn owner(&self) -> HWND {
        match &self.source {
            Source::View(view) => unsafe { view.GetWindow() }.unwrap_or_default(),
            Source::Selection { owner, .. } => *owner,
        }
    }
    fn query(&self, menu: HMENU, mut idx: u32, first: u32, last: u32, flags: u32) -> Result<u32> {
        if last.saturating_sub(first) < FULL_OFFSET + 1 {
            return Err(err("菜单命令范围不足"));
        }
        self.first.set(first);
        self.last.set(last);
        self.flags.set(flags);
        self.loaded.set(false);
        self.roots.borrow_mut().clear();
        self.aliases.borrow_mut().clear();
        self.groups.borrow_mut().clear();
        self.group_menus.borrow_mut().clear();
        let options = super::menu::context_menu_options();
        let allowed = options.native_first_page_apps.as_deref().unwrap_or(&[]);
        let groups = if allowed.iter().any(|key| key.starts_with("legacy:")) {
            super::legacy_commands::selected(self.view()?, self.background, allowed)?
        } else {
            Vec::new()
        };
        let span = (last - first - FULL_OFFSET + 1) / (groups.len() as u32 + 1);
        if span == 0 {
            return Err(err("菜单命令范围不足"));
        }
        self.full_last.set(first + FULL_OFFSET + span - 1);
        let mut attrs = 0;
        let mut count = 0;
        let mut drive = false;
        let mut extension = String::new();
        let mut selection_has_folder = false;
        if !self.background {
            let items = self.selection()?;
            count = unsafe { items.GetCount() }?;
            let mask = SFGAO_FOLDER
                | SFGAO_FILESYSTEM
                | SFGAO_STREAM
                | SFGAO_LINK
                | SFGAO_CANRENAME
                | SFGAO_CANCOPY
                | SFGAO_CANDELETE
                | SFGAO_CANMOVE;
            attrs = unsafe { items.GetAttributes(SIATTRIBFLAGS_AND, mask) }?.0;
            if count > 1 {
                selection_has_folder =
                    unsafe { items.GetAttributes(SIATTRIBFLAGS_OR, SFGAO_FOLDER) }?
                        .contains(SFGAO_FOLDER);
            }
            if count == 1 && attrs & SFGAO_FILESYSTEM.0 != 0 {
                if let Ok(p) = unsafe { items.GetItemAt(0)?.GetDisplayName(SIGDN_FILESYSPATH) } {
                    let path = unsafe { super::take(p) };
                    let path = std::path::Path::new(&path);
                    drive = path.parent().is_none();
                    if let Some(suffix) = path.extension().and_then(|s| s.to_str()) {
                        extension = format!(".{suffix}");
                    }
                }
            }
        }
        let fs = attrs & SFGAO_FILESYSTEM.0 != 0;
        let folder = count == 1 && fs && attrs & SFGAO_FOLDER.0 != 0 && attrs & SFGAO_STREAM.0 == 0;
        let selection = !self.background && count > 0;
        let file = count == 1 && fs && !folder;
        let rotate = file && super::legacy_commands::has_image_rotation(&extension);
        let mut rows = Vec::new();
        if selection {
            for (id, available) in [
                (0, attrs & SFGAO_CANMOVE.0 != 0),
                (1, attrs & SFGAO_CANCOPY.0 != 0),
                (2, count == 1 && attrs & SFGAO_CANRENAME.0 != 0),
                (3, fs && !folder && count == 1),
                (4, attrs & SFGAO_CANDELETE.0 != 0),
                (5, count == 1),
                (25, file && attrs & SFGAO_LINK.0 != 0),
                (
                    26,
                    file && super::legacy_commands::has_static_verb(&extension, "runas"),
                ),
                (6, file && !extension.eq_ignore_ascii_case(".exe")),
                (
                    24,
                    file && attrs & (SFGAO_FOLDER.0 | SFGAO_STREAM.0)
                        == (SFGAO_FOLDER.0 | SFGAO_STREAM.0)
                        && super::legacy_commands::system_handler("CompressedFile.extract")
                            .is_some(),
                ),
                (
                    27,
                    file && super::legacy_commands::has_static_verb(
                        &extension,
                        "setdesktopwallpaper",
                    ),
                ),
                (28, rotate),
                (29, rotate),
                (20, folder),
                (21, folder),
                (19, folder),
                (9, folder && !drive),
                (
                    16,
                    fs && !folder
                        && !selection_has_folder
                        && super::legacy_commands::system_handler("ModernShareFlyout").is_some(),
                ),
                (
                    17,
                    count == 1
                        && fs
                        && !folder
                        && super::legacy_commands::system_handler("PinToHomeFile").is_some(),
                ),
                (
                    18,
                    fs && super::legacy_commands::system_handler("CompressTo").is_some(),
                ),
                (7, fs),
                (8, true),
            ] {
                if available {
                    rows.push(id);
                }
            }
        } else if self.background {
            rows.extend([11, 12, 13, 14]);
            if super::file_transfer::filesystem_folder_path(self.view()?)
                .ok()
                .flatten()
                .is_some()
            {
                if super::file_transfer::has_paste_content() {
                    rows.push(10);
                }
                rows.push(15);
            }
            if matches!(super::file_transfer::paste_folder(self.view()?), Ok(None)) {
                rows.extend([22, 23]);
            } else {
                rows.push(8);
            }
        }
        rows.push(MORE);
        for offset in rows {
            let r = COMMANDS.iter().find(|r| r.offset == offset).unwrap();
            if self.background && offset==14{
                unsafe{InsertMenuW(menu,idx,MF_BYPOSITION|MF_SEPARATOR,0,None)}?;
                idx+=1;
            }
            if offset == MORE {
                for (n, (label, context)) in groups.iter().enumerate() {
                    let start = self.full_last.get() + 1 + n as u32 * span;
                    let stage = unsafe { CreatePopupMenu() }?;
                    let hr = unsafe {
                        context.QueryContextMenu(
                            stage,
                            0,
                            start,
                            start + span - 1,
                            flags & !CMF_OPTIMIZEFORINVOKE,
                        )
                    };
                    if hr.is_err()
                        || hr.0 as u32 & 0xffff > span
                        || unsafe { GetMenuItemCount(Some(stage)) } <= 0
                    {
                        let _ = unsafe { DestroyMenu(stage) };
                        continue;
                    }
                    let mut sub = stage;
                    if unsafe { GetMenuItemCount(Some(stage)) } == 1 {
                        let child = unsafe { GetSubMenu(stage, 0) };
                        if !child.is_invalid()
                            && unsafe { RemoveMenu(stage, 0, MF_BYPOSITION) }.is_ok()
                        {
                            sub = child;
                            let _ = unsafe { DestroyMenu(stage) };
                        }
                    }
                    let title: Vec<_> = label
                        .replace('&', "&&")
                        .encode_utf16()
                        .chain(Some(0))
                        .collect();
                    if let Err(e) = unsafe {
                        InsertMenuW(
                            menu,
                            idx,
                            MF_BYPOSITION | MF_POPUP,
                            sub.0 as usize,
                            windows::core::PCWSTR(title.as_ptr()),
                        )
                    } {
                        let _ = unsafe { DestroyMenu(sub) };
                        return Err(e);
                    }
                    let group_index = self.groups.borrow().len();
                    self.groups.borrow_mut().push(std::rc::Rc::new(AppGroup {
                        context: context.clone(),
                        first: start,
                        last: start + span - 1,
                        root: sub,
                    }));
                    self.group_menus
                        .borrow_mut()
                        .insert(sub.0 as usize, group_index);
                    idx += 1;
                }
                unsafe { InsertMenuW(menu, idx, MF_BYPOSITION | MF_SEPARATOR, 0, None) }?;
                idx += 1;
            }
            let mut text: Vec<_> = zsdesk_core::i18n::text(r.label)
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut i = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID | MIIM_STRING,
                wID: first + offset,
                dwTypeData: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            let resource = match r.verb {
                "openas" => "OpenWith",
                "windows.modernshare" => "ModernShare",
                "sharewith" => "ModernShareFlyout",
                other => other,
            };
            let application=if r.verb=="open"&&file{super::legacy_commands::opening_application_icon(&extension)}else{None};
            if let Some(icon) = application.or_else(||super::legacy_commands::system_icon(resource)).and_then(|resource| {
                super::packaged_overlay::bitmap_from_icon(&resource, self.owner()).ok()
            }) {
                i.fMask |= MIIM_BITMAP;
                i.hbmpItem = icon.0;
                self.icons.borrow_mut().push(icon);
            }
            if r.cascade {
                i.hSubMenu = unsafe { CreatePopupMenu() }?;
                i.fMask |= MIIM_SUBMENU;
            }
            if let Err(e) = unsafe { InsertMenuItemW(menu, idx, true, &i) } {
                if r.cascade {
                    let _ = unsafe { DestroyMenu(i.hSubMenu) };
                }
                return Err(e);
            }
            if r.cascade {
                self.roots.borrow_mut().insert(i.hSubMenu.0 as usize, *r);
                if offset == MORE {
                    self.full.set(i.hSubMenu);
                }
            }
            idx += 1;
        }
        Ok(last - first + 1)
    }
    fn full_context(&self) -> Result<&IContextMenu> {
        if self.context.get().is_none() {
            let ctx = unsafe {
                self.view()?.GetItemObject(if self.background {
                    SVGIO_BACKGROUND
                } else {
                    SVGIO_SELECTION
                })
            }?;
            let _ = self.context.set(ctx);
        }
        let context = self.context.get().unwrap();
        if !self.loaded.get() {
            let full = self.full.get();
            if full.is_invalid() {
                return Err(err("缺少完整菜单"));
            }
            unsafe {
                context.QueryContextMenu(
                    full,
                    0,
                    self.first.get() + FULL_OFFSET,
                    self.full_last.get(),
                    self.flags.get() & !CMF_OPTIMIZEFORINVOKE,
                )
            }
            .ok()?;
            self.loaded.set(true);
        }
        Ok(context)
    }
    fn apply_message(context: &IContextMenu, msg: u32, wp: WPARAM, lp: LPARAM) -> Result<LRESULT> {
        if let Ok(v) = context.cast::<IContextMenu3>() {
            let mut r = LRESULT(0);
            unsafe { v.HandleMenuMsg2(msg, wp, lp, Some(&mut r)) }?;
            return Ok(r);
        }
        if let Ok(v) = context.cast::<IContextMenu2>() {
            unsafe { v.HandleMenuMsg(msg, wp, lp) }?;
        }
        Ok(LRESULT(0))
    }
    fn system_command(&self, name: &str) -> Result<IExplorerCommand> {
        use windows::Win32::System::{
            Com::{CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, CoCreateInstance},
            Ole::IObjectWithSite,
        };
        let guid =
            super::legacy_commands::system_handler(name).ok_or_else(|| err("系统未注册此命令"))?;
        let c: IExplorerCommand =
            unsafe { CoCreateInstance(&guid, None, CLSCTX_INPROC_SERVER | CLSCTX_LOCAL_SERVER) }?;
        if let Ok(site) = c.cast::<IObjectWithSite>() {
            let _ = unsafe { site.SetSite(self.view()?) };
        }
        Ok(c)
    }
    fn selection(&self) -> Result<IShellItemArray> {
        match &self.source {
            Source::View(view) => unsafe { view.cast::<IFolderView2>()?.GetSelection(false) },
            Source::Selection { items, .. } => Ok(items.clone()),
        }
    }
    fn system_children(&self, popup: HMENU, parent: &IExplorerCommand) -> Result<()> {
        if self.sys_ready.borrow().contains(&(popup.0 as usize)) {
            return Ok(());
        }
        let selection = self.selection()?;
        let children = unsafe { parent.EnumSubCommands() }?;
        loop {
            let mut one = [None];
            let mut fetched = 0;
            unsafe { children.Next(&mut one, Some(&mut fetched)) }.ok()?;
            let Some(command) = one[0].take().filter(|_| fetched == 1) else {
                break;
            };
            if self.sys_nodes.borrow().len() >= (RESERVED - SYSTEM_START) as usize {
                return Err(err("系统子菜单项目过多"));
            }
            let state =
                unsafe { command.GetState(&selection, false) }.unwrap_or(ECS_DISABLED.0 as u32);
            if state & ECS_HIDDEN.0 as u32 != 0 {
                continue;
            }
            let flags = unsafe { command.GetFlags() }.unwrap_or(0);
            if flags & ECF_ISSEPARATOR.0 as u32 != 0 {
                unsafe { AppendMenuW(popup, MF_SEPARATOR, 0, None) }?;
                continue;
            }
            let Ok(title) = (unsafe { command.GetTitle(&selection) }) else {
                continue;
            };
            let title = unsafe { super::take(title) };
            let mut wide: Vec<_> = title
                .replace('&', "&&")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let id = self.first.get() + SYSTEM_START + self.sys_nodes.borrow().len() as u32;
            let mut i = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID | MIIM_STATE | MIIM_STRING,
                wID: id,
                dwTypeData: PWSTR(wide.as_mut_ptr()),
                ..Default::default()
            };
            if state & ECS_DISABLED.0 as u32 != 0 {
                i.fState |= MFS_DISABLED;
            }
            if state & ECS_CHECKED.0 as u32 != 0 {
                i.fState |= MFS_CHECKED;
            }
            if flags & ECF_HASSUBCOMMANDS.0 as u32 != 0 {
                i.fMask |= MIIM_SUBMENU;
                i.hSubMenu = unsafe { CreatePopupMenu() }?;
            }
            if let Err(e) = unsafe { InsertMenuItemW(popup, u32::MAX, true, &i) } {
                if !i.hSubMenu.is_invalid() {
                    let _ = unsafe { DestroyMenu(i.hSubMenu) };
                }
                return Err(e);
            }
            if !i.hSubMenu.is_invalid() {
                self.sys_groups
                    .borrow_mut()
                    .insert(i.hSubMenu.0 as usize, command.clone());
            }
            self.sys_nodes.borrow_mut().push(command);
        }
        self.sys_ready.borrow_mut().insert(popup.0 as usize);
        Ok(())
    }
    fn matching(&self, row: Row) -> Result<Option<(MENUITEMINFOW, u32)>> {
        let c = self.full_context()?;
        let m = self.full.get();
        for p in 0..unsafe { GetMenuItemCount(Some(m)) }.max(0) as u32 {
            let (i, t) = info(m, p)?;
            let mut s = [0u16; 180];
            let has = (self.first.get() + FULL_OFFSET..=self.full_last.get()).contains(&i.wID)
                && unsafe {
                    c.GetCommandString(
                        (i.wID - self.first.get() - FULL_OFFSET) as usize,
                        GCS_VERBW,
                        None,
                        PSTR(s.as_mut_ptr().cast()),
                        s.len() as u32,
                    )
                }
                .is_ok();
            let verb = if has {
                String::from_utf16_lossy(&s[..s.iter().position(|v| *v == 0).unwrap_or(s.len())])
            } else {
                String::new()
            };
            let label =
                String::from_utf16_lossy(&t[..t.iter().position(|v| *v == 0).unwrap_or(t.len())]);
            let trim = |v: &str| {
                v.split('\t')
                    .next()
                    .unwrap_or(v)
                    .split("(&")
                    .next()
                    .unwrap_or(v)
                    .replace('&', "")
                    .trim_end_matches(['…', '.'])
                    .trim()
                    .to_lowercase()
            };
            if verb.eq_ignore_ascii_case(row.verb)
                || row.verb == "pintostartscreen"
                    && matches!(
                        verb.to_lowercase().as_str(),
                        "unpinfromstart" | "unpinfromstartscreen"
                    )
                || trim(&label) == trim(zsdesk_core::i18n::text(row.label))
            {
                return Ok(Some((i, p)));
            }
        }
        Ok(None)
    }
    fn fill_alias(&self, popup: HMENU, src: Alias) -> Result<()> {
        // Keep Shell-owned handles on the original tree; this displayed copy owns
        // its own child handles. Command IDs still belong to the Shell context.
        let old: Vec<_> = self
            .aliases
            .borrow()
            .keys()
            .copied()
            .filter(|h| !unsafe { IsMenu(HMENU(*h as *mut _)) }.as_bool())
            .collect();
        for h in old {
            self.aliases.borrow_mut().remove(&h);
        }
        while unsafe { GetMenuItemCount(Some(popup)) } > 0 {
            unsafe { DeleteMenu(popup, 0, MF_BYPOSITION) }?;
        }
        let count = unsafe { GetMenuItemCount(Some(src.source)) }.max(0) as u32;
        if count > 4096 {
            return Err(err("子菜单项目过多"));
        }
        for index in 0..count {
            let (mut i, _text) = info(src.source, index)?;
            let original = i.hSubMenu;
            if !original.is_invalid() {
                i.hSubMenu = unsafe { CreatePopupMenu() }?;
            }
            if let Err(e) = unsafe { InsertMenuItemW(popup, index, true, &i) } {
                if !original.is_invalid() {
                    let _ = unsafe { DestroyMenu(i.hSubMenu) };
                }
                return Err(e);
            }
            if !original.is_invalid() {
                self.aliases.borrow_mut().insert(
                    i.hSubMenu.0 as usize,
                    Alias {
                        source: original,
                        position: index,
                    },
                );
            }
        }
        Ok(())
    }
    fn message(&self, msg: u32, wp: WPARAM, lp: LPARAM) -> Result<LRESULT> {
        let popup = HMENU(wp.0 as *mut _);
        let which = match msg {
            WM_INITMENUPOPUP | WM_UNINITMENUPOPUP => self.group_menus.borrow().get(&wp.0).copied(),
            WM_MENUCHAR | WM_MENUSELECT => self.group_menus.borrow().get(&(lp.0 as usize)).copied(),
            WM_MEASUREITEM | WM_DRAWITEM if lp.0 != 0 => {
                use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODT_MENU};
                let (kind, id) = if msg == WM_MEASUREITEM {
                    let s = unsafe { &*(lp.0 as *const MEASUREITEMSTRUCT) };
                    (s.CtlType, s.itemID)
                } else {
                    let s = unsafe { &*(lp.0 as *const DRAWITEMSTRUCT) };
                    (s.CtlType, s.itemID)
                };
                (kind == ODT_MENU)
                    .then(|| {
                        self.groups
                            .borrow()
                            .iter()
                            .position(|g| (g.first..=g.last).contains(&id))
                    })
                    .flatten()
            }
            _ => None,
        };
        if let Some(n) = which {
            let g = self.groups.borrow()[n].clone();
            let param = if popup == g.root && matches!(msg, WM_INITMENUPOPUP | WM_UNINITMENUPOPUP) {
                LPARAM(0)
            } else {
                lp
            };
            let r = Self::apply_message(&g.context, msg, wp, param)?;
            if msg == WM_INITMENUPOPUP {
                for i in 0..unsafe { GetMenuItemCount(Some(popup)) }.max(0) {
                    let sub = unsafe { GetSubMenu(popup, i) };
                    if !sub.is_invalid() {
                        self.group_menus.borrow_mut().insert(sub.0 as usize, n);
                    }
                }
            }
            return Ok(r);
        }
        if msg == WM_INITMENUPOPUP || msg == WM_UNINITMENUPOPUP {
            let root = self.roots.borrow().get(&wp.0).copied();
            if msg == WM_INITMENUPOPUP {
                let nested = self.sys_groups.borrow().get(&wp.0).cloned();
                let known = root.and_then(|r| match r.verb {
                    "compressto" => Some("CompressTo"),
                    "sharewith" => Some("ModernShareFlyout"),
                    _ => None,
                });
                if let Some(c) = nested.or_else(|| known.and_then(|k| self.system_command(k).ok()))
                {
                    self.system_children(popup, &c)?;
                    return Ok(LRESULT(0));
                }
            }
            if root.is_some_and(|r| r.verb == "newfolder") {
                if self.templates.get().is_none() {
                    if msg != WM_INITMENUPOPUP {
                        return Ok(LRESULT(0));
                    }
                    let ctx = super::new_menu::context_for_view(self.view()?)?;
                    let menu = unsafe { CreatePopupMenu() }?;
                    let mut templates = Templates {
                        ctx,
                        menu,
                        child: HMENU::default(),
                    };
                    let start = self.first.get() + RESERVED;
                    unsafe {
                        templates.ctx.QueryContextMenu(
                            menu,
                            0,
                            start,
                            start + NEW_COUNT - 1,
                            CMF_NORMAL,
                        )
                    }
                    .ok()?;
                    templates.child = (0..unsafe { GetMenuItemCount(Some(menu)) }.max(0))
                        .map(|n| unsafe { GetSubMenu(menu, n) })
                        .find(|h| !h.is_invalid())
                        .ok_or_else(|| err("该位置没有新建模板"))?;
                    let _ = self.templates.set(templates);
                }
                let t = self.templates.get().unwrap();
                let src = Alias {
                    source: t.child,
                    position: 0,
                };
                let value =
                    Self::apply_message(&t.ctx, msg, WPARAM(src.source.0 as usize), LPARAM(0))?;
                if msg == WM_INITMENUPOPUP {
                    self.fill_alias(popup, src)?;
                }
                return Ok(value);
            }
            if msg == WM_INITMENUPOPUP && root.is_some_and(|r| r.offset == MORE) {
                self.full_context()?;
            }
            let mut alias = self.aliases.borrow().get(&wp.0).copied();
            if msg == WM_INITMENUPOPUP && root.is_some_and(|r| r.offset != MORE) {
                if let Some((i, position)) = self.matching(root.unwrap())? {
                    if !i.hSubMenu.is_invalid() {
                        alias = Some(Alias {
                            source: i.hSubMenu,
                            position,
                        });
                        self.aliases.borrow_mut().insert(wp.0, alias.unwrap());
                    }
                }
            }
            if let Some(a) = alias {
                let result = Self::apply_message(
                    self.full_context()?,
                    msg,
                    WPARAM(a.source.0 as usize),
                    LPARAM(a.position as isize),
                )?;
                if msg == WM_INITMENUPOPUP {
                    self.fill_alias(popup, a)?;
                }
                return Ok(result);
            }
        }
        if let Some(c) = self.context.get() {
            return Self::apply_message(c, msg, wp, lp);
        }
        Ok(LRESULT(0))
    }
}
impl IContextMenu_Impl for Compact_Impl {
    fn QueryContextMenu(&self, m: HMENU, idx: u32, first: u32, last: u32, flags: u32) -> HRESULT {
        self.query(m, idx, first, last, flags)
            .map_or_else(|e| e.code(), |n| HRESULT(n as i32))
    }
    fn GetCommandString(
        &self,
        offset: usize,
        kind: u32,
        reserved: *const u32,
        target: PSTR,
        count: u32,
    ) -> Result<()> {
        if (SYSTEM_START as usize..RESERVED as usize).contains(&offset) {
            return Err(Error::from_hresult(windows::Win32::Foundation::E_NOTIMPL));
        }
        let id = self.first.get().saturating_add(offset as u32);
        let group = self
            .groups
            .borrow()
            .iter()
            .find(|g| (g.first..=g.last).contains(&id))
            .cloned();
        if let Some(g) = group {
            return unsafe {
                g.context.GetCommandString(
                    (id - g.first) as usize,
                    kind,
                    Some(reserved),
                    target,
                    count,
                )
            };
        }
        if (RESERVED as usize..FULL_OFFSET as usize).contains(&offset) {
            let t = self.templates.get().ok_or_else(|| err("模板尚未打开"))?;
            return unsafe {
                t.ctx.GetCommandString(
                    offset - RESERVED as usize,
                    kind,
                    Some(reserved),
                    target,
                    count,
                )
            };
        }
        if offset >= FULL_OFFSET as usize {
            let c = self
                .context
                .get()
                .filter(|_| self.loaded.get())
                .ok_or_else(|| err("完整菜单尚未打开"))?;
            return unsafe {
                c.GetCommandString(
                    offset - FULL_OFFSET as usize,
                    kind,
                    Some(reserved),
                    target,
                    count,
                )
            };
        }
        let row = COMMANDS
            .iter()
            .find(|r| r.offset as usize == offset)
            .ok_or_else(|| err("未知菜单命令"))?;
        if matches!(kind, GCS_VALIDATEA | GCS_VALIDATEW) {
            return Ok(());
        }
        unsafe {
            super::packaged_overlay::write_command_text(
                target,
                count,
                row.verb,
                kind & GCS_UNICODE != 0,
            )
        }
    }
    fn InvokeCommand(&self, p: *const CMINVOKECOMMANDINFO) -> Result<()> {
        if p.is_null() {
            return Err(Error::from_hresult(windows::Win32::Foundation::E_POINTER));
        }
        let base = unsafe { &*p };
        let offset = base.lpVerb.0 as usize;
        if (SYSTEM_START as usize..RESERVED as usize).contains(&offset) {
            let c = self
                .sys_nodes
                .borrow()
                .get(offset - SYSTEM_START as usize)
                .cloned()
                .ok_or_else(|| err("命令已关闭"))?;
            let selection = self.selection()?;
            let state = unsafe { c.GetState(&selection, true) }?;
            if state & (ECS_DISABLED.0 | ECS_HIDDEN.0) as u32 != 0 {
                return Err(err("当前选择不支持此命令"));
            }
            return unsafe { c.Invoke(&selection, None) };
        }
        if offset > 0xffff {
            return unsafe { self.full_context()?.InvokeCommand(p) };
        }
        let id = self.first.get().saturating_add(offset as u32);
        let group = self
            .groups
            .borrow()
            .iter()
            .find(|g| (g.first..=g.last).contains(&id))
            .cloned();
        let template = (RESERVED as usize..FULL_OFFSET as usize).contains(&offset);
        let ctx = if template {
            self.templates
                .get()
                .ok_or_else(|| err("模板已改变"))?
                .ctx
                .clone()
        } else if let Some(g) = &group {
            g.context.clone()
        } else {
            self.full_context()?.clone()
        };
        let target = if template {
            offset - RESERVED as usize
        } else if let Some(g) = group {
            (id - g.first) as usize
        } else if offset >= FULL_OFFSET as usize {
            offset - FULL_OFFSET as usize
        } else {
            let r = COMMANDS
                .iter()
                .find(|r| r.offset as usize == offset)
                .ok_or_else(|| err("未知菜单命令"))?;
            let (i, _) = self
                .matching(*r)?
                .ok_or_else(|| err("当前选择不支持这项命令"))?;
            i.wID
                .checked_sub(self.first.get() + FULL_OFFSET)
                .ok_or_else(|| err("命令标识已改变"))? as usize
        };
        if base.cbSize as usize >= size_of::<CMINVOKECOMMANDINFOEX>() {
            let mut copy = unsafe { ptr::read(p.cast::<CMINVOKECOMMANDINFOEX>()) };
            copy.lpVerb = PCSTR(target as *const _);
            copy.lpVerbW = windows::core::PCWSTR(target as *const _);
            return unsafe { ctx.InvokeCommand((&copy as *const CMINVOKECOMMANDINFOEX).cast()) };
        }
        let mut copy = *base;
        copy.lpVerb = PCSTR(target as *const _);
        unsafe { ctx.InvokeCommand(&copy) }
    }
}
impl IContextMenu2_Impl for Compact_Impl {
    fn HandleMenuMsg(&self, m: u32, w: WPARAM, l: LPARAM) -> Result<()> {
        self.message(m, w, l).map(|_| ())
    }
}
impl IContextMenu3_Impl for Compact_Impl {
    fn HandleMenuMsg2(&self, m: u32, w: WPARAM, l: LPARAM, r: *mut LRESULT) -> Result<()> {
        let v = self.message(m, w, l)?;
        if !r.is_null() {
            unsafe {
                *r = v;
            }
        }
        Ok(())
    }
}
