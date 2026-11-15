//! Real ResultsFolder lifecycle with an owned, hidden browser and no input.
use super::*;
use anyhow::{Context,ensure};
use std::{fs,io::Write,thread,time::{Duration,Instant}};
use windows::core::BOOL;
use windows::Win32::{Foundation::{HINSTANCE,HMODULE,LRESULT,FreeLibrary},System::{LibraryLoader::{GetModuleHandleW,LoadLibraryW,GetProcAddress},Ole::{OleInitialize,OleUninitialize}},
    UI::{Controls::{LVM_GETBKCOLOR,LVM_GETTEXTCOLOR},WindowsAndMessaging::*}};
#[link(name="user32")]unsafe extern "system"{
    fn PrintWindow(hwnd:HWND,dc:windows::Win32::Graphics::Gdi::HDC,flags:u32)->BOOL;
}
type PreferredMode=unsafe extern "system" fn(i32)->i32;
type FlushThemes=unsafe extern "system" fn();
type ShouldAppsUseDarkMode=unsafe extern "system" fn()->bool;
struct ProcessTheme{module:HMODULE,preferred:PreferredMode,flush:FlushThemes,should_dark:ShouldAppsUseDarkMode,previous:i32,previous_dark:bool}
impl ProcessTheme{
    fn capture()->anyhow::Result<Self>{
        ensure!(crate::theme::os_build()>=18362,"preferred color-scheme fixture requires Windows build 18362 or later");
        let module=unsafe{LoadLibraryW(windows::core::w!("uxtheme.dll"))}?;
        let preferred=unsafe{GetProcAddress(module,windows::core::PCSTR(135usize as*const u8))};
        let flush=unsafe{GetProcAddress(module,windows::core::PCSTR(136usize as*const u8))};
        let should_dark=unsafe{GetProcAddress(module,windows::core::PCSTR(132usize as*const u8))};
        let (Some(preferred),Some(flush),Some(should_dark))=(preferred,flush,should_dark)else{let _=unsafe{FreeLibrary(module)};anyhow::bail!("preferred color-scheme API unavailable");};
        // Same build/signature boundary as production theme.rs. This changes
        // only the dedicated test process and records the exact prior mode.
        let preferred:PreferredMode=unsafe{std::mem::transmute(preferred)};let flush:FlushThemes=unsafe{std::mem::transmute(flush)};
        // Microsoft PowerToys ZoomIt/Utility.cpp uses ordinal 132 as bool().
        let should_dark:ShouldAppsUseDarkMode=unsafe{std::mem::transmute(should_dark)};
        let previous_dark=crate::theme::menus_dark();let previous=unsafe{preferred(2)};
        Ok(Self{module,preferred,flush,should_dark,previous,previous_dark})
    }
    fn observed_dark(&self)->bool{unsafe{(self.should_dark)()}}
}
impl Drop for ProcessTheme{fn drop(&mut self){
    crate::theme::allow_dark_mode(self.previous_dark);
    unsafe{(self.preferred)(self.previous);(self.flush)();let _=FreeLibrary(self.module);}
}}
fn scheme(dark:bool){crate::theme::configure_color_scheme(if dark{zsdesk_core::model::explorer::ColorScheme::Dark}else{zsdesk_core::model::explorer::ColorScheme::Light});}

unsafe extern "system" fn window_proc(hwnd:HWND,message:u32,wp:WPARAM,lp:LPARAM)->LRESULT{
    match message{
        WM_SHELL_NAVIGATED|WM_SHELL_FOLDER_INVOKED=>{if lp.0!=0{unsafe{drop(Box::from_raw(lp.0 as*mut ShellLocation));}}LRESULT(0)},
        WM_SHELL_SELECTION_CHANGED=>{unsafe{super::super::events::acknowledge_selection_changed(lp.0);}LRESULT(0)},
        _=>unsafe{DefWindowProcW(hwnd,message,wp,lp)},
    }
}
fn pump(ms:u64){let until=Instant::now()+Duration::from_millis(ms);while Instant::now()<until{
    let mut message=MSG::default();for _ in 0..96{if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}
        if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}
    thread::sleep(Duration::from_millis(2));
}}
fn class(hwnd:HWND)->String{let mut name=[0u16;96];let n=unsafe{GetClassNameW(hwnd,&mut name)}.max(0)as usize;String::from_utf16_lossy(&name[..n])}
fn renderers(view:HWND)->Vec<(HWND,String,Option<view_background::Colors>)>{
    unsafe extern "system" fn visit(hwnd:HWND,lp:LPARAM)->BOOL{
        let kind=class(hwnd);
        if matches!(kind.as_str(),"DirectUIHWND"|"SysListView32"){unsafe{&mut*(lp.0 as*mut Vec<(HWND,String,Option<view_background::Colors>)>)}.push((hwnd,kind,view_background::installed_colors(hwnd)));}
        true.into()
    }
    let mut found=Vec::new();let _=unsafe{EnumChildWindows(Some(view),Some(visit),LPARAM((&mut found as*mut Vec<_>)as isize))};found
}
fn child_tree(view:HWND)->Vec<serde_json::Value>{
    unsafe extern "system" fn visit(hwnd:HWND,lp:LPARAM)->BOOL{
        let rows=unsafe{&mut*(lp.0 as*mut Vec<serde_json::Value>)};if rows.len()>=64{return false.into();}
        let mut rect=RECT::default();let _=unsafe{GetClientRect(hwnd,&mut rect)};
        let kind=class(hwnd);let list=kind=="SysListView32";
        rows.push(serde_json::json!({"class":kind,"parent_class":unsafe{GetParent(hwnd)}.ok().map(class),"client_size":[rect.right,rect.bottom],"visible":unsafe{IsWindowVisible(hwnd)}.as_bool(),"style":unsafe{GetWindowLongPtrW(hwnd,GWL_STYLE)}as u64,"sys_set_redraw_property":unsafe{GetPropW(hwnd,windows::core::w!("SysSetRedraw"))}.0 as usize,"actual_bkcolor":list.then(||unsafe{SendMessageW(hwnd,LVM_GETBKCOLOR,None,None)}.0 as u32),"actual_textcolor":list.then(||unsafe{SendMessageW(hwnd,LVM_GETTEXTCOLOR,None,None)}.0 as u32),"actual_itemcount":list.then(||unsafe{SendMessageW(hwnd,windows::Win32::UI::Controls::LVM_GETITEMCOUNT,None,None)}.0),"configured_background":view_background::installed_colors(hwnd).map(|value|value.background),"ready_theme":view_background::applied_theme(hwnd)}));true.into()
    }
    let mut rows=Vec::new();let _=unsafe{EnumChildWindows(Some(view),Some(visit),LPARAM((&mut rows as*mut Vec<serde_json::Value>)as isize))};rows
}
fn capture_renderer(view:HWND)->anyhow::Result<(usize,Vec<u8>)>{
    let (window,kind,rect)=renderers(view).into_iter().filter_map(|(hwnd,kind,_)|{
        let mut rect=RECT::default();unsafe{GetClientRect(hwnd,&mut rect)}.ok().filter(|_|rect.right>0&&rect.bottom>0).map(|_|(hwnd,kind,rect))
    }).max_by_key(|(_,kind,rect)|(kind=="SysListView32",rect.right*rect.bottom)).context("native content renderer")?;
    ensure!(rect.right>=128&&rect.bottom>=160&&rect.right<=2048&&rect.bottom<=2048,"unexpected fixture renderer size");
    let surface=crate::gfx::Surface::new(rect.right,rect.bottom).context("native selection raster")?;
    let bytes=(rect.right*rect.bottom*4)as usize;let mut previous=None;
    let mut capture_methods=Vec::new();
    for method in 0..3{
        unsafe{std::ptr::write_bytes(surface.bits,0x7d,bytes);match method{
            0=>{let _=PrintWindow(window,surface.dc,1|PW_RENDERFULLCONTENT);},
            1=>{SendMessageW(window,WM_PRINT,Some(WPARAM(surface.dc.0 as usize)),Some(LPARAM((PRF_CLIENT|PRF_ERASEBKGND|PRF_CHILDREN)as isize)));},
            _=>{SendMessageW(window,WM_PRINTCLIENT,Some(WPARAM(surface.dc.0 as usize)),Some(LPARAM((PRF_CLIENT|PRF_ERASEBKGND)as isize)));},
        }}
        let sample=unsafe{std::slice::from_raw_parts(surface.bits,bytes)};capture_methods.push(serde_json::json!({"method":method,"center_rgb":background_rgb(sample,rect.right as usize)}));
    }
    println!("{}",serde_json::json!({"phase":"renderer-capture-methods","class":kind,"size":[rect.right,rect.bottom],"samples":capture_methods}));
    for _ in 0..8{
        unsafe{std::ptr::write_bytes(surface.bits,0x7d,bytes);}
        if kind=="SysListView32"{
            unsafe{SendMessageW(window,WM_PRINT,Some(WPARAM(surface.dc.0 as usize)),Some(LPARAM((PRF_CLIENT|PRF_ERASEBKGND|PRF_CHILDREN)as isize)));}
        }else{ensure!(unsafe{PrintWindow(window,surface.dc,1|PW_RENDERFULLCONTENT)}.as_bool(),"native renderer rejected PrintWindow");}
        let mut pixels=unsafe{std::slice::from_raw_parts(surface.bits,bytes)}.to_vec();
        ensure!(pixels.chunks_exact(4).any(|pixel|pixel[..3]!=[0x7d;3]),"native renderer did not paint its raster");
        // GDI PrintWindow does not promise alpha, but this renderer is opaque.
        for pixel in pixels.chunks_exact_mut(4){pixel[3]=255;}
        if previous.as_ref()==Some(&pixels){return Ok((rect.right as usize,pixels));}
        previous=Some(pixels);pump(20);
    }
    anyhow::bail!("native renderer did not settle to a stable raster")
}
fn background_rgb(pixels:&[u8],width:usize)->[u8;3]{
    let height=pixels.len()/(width*4);let start=((height/2)*width+width/2)*4;
    [pixels[start+2],pixels[start+1],pixels[start]]
}
fn save_raster(path:&Path,pixels:&[u8],width:usize)->anyhow::Result<()>{
    use std::os::windows::fs::MetadataExt;
    if path.exists(){ensure!(fs::symlink_metadata(path)?.file_attributes()&0x400==0,"raster output is a reparse point");}
    let height=pixels.len()/(width*4);ensure!(width>0&&height>0&&pixels.len()==width*height*4,"invalid raster dimensions");
    let mut bmp=Vec::with_capacity(54+pixels.len());bmp.extend(b"BM");bmp.extend((54u32+pixels.len()as u32).to_le_bytes());bmp.extend([0;4]);bmp.extend(54u32.to_le_bytes());
    bmp.extend(40u32.to_le_bytes());bmp.extend((width as i32).to_le_bytes());bmp.extend((-(height as i32)).to_le_bytes());bmp.extend(1u16.to_le_bytes());bmp.extend(32u16.to_le_bytes());
    bmp.extend([0;4]);bmp.extend((pixels.len()as u32).to_le_bytes());bmp.extend([0;16]);bmp.extend(pixels);fs::write(path,bmp)?;Ok(())
}
fn verify_background(pixels:&[u8],width:usize,dark:bool,require_ink:bool)->anyhow::Result<[u8;3]>{
    let color=background_rgb(pixels,width);let average=color.iter().map(|value|u32::from(*value)).sum::<u32>()/3;
    ensure!(if dark{(8..96).contains(&average)}else{average>180},"native search content does not follow the actual process color scheme: dark={dark}, background={color:?}");
    // Reject a blank/cached PrintWindow frame even if its solid black happens
    // to meet the dark-background threshold.
    let mut ink=0;for pixel in pixels[..pixels.len().min(width*120*4)].chunks_exact(4){if [pixel[2],pixel[1],pixel[0]].iter().zip(color).any(|(a,b)|a.abs_diff(b)>45){ink+=1;}}
    ensure!(!require_ink||ink>100,"native renderer raster contains no visible rows or headers");Ok(color)
}
struct Raster{width:usize,pixels:Vec<u8>,background:[u8;3]}
fn diagnostic_raster(view:HWND,path:&Path,dark:bool,require_ink:bool,phase:&str,failures:&mut Vec<String>)->Option<Raster>{
    match capture_renderer(view){
        Ok((width,pixels))=>{
            if let Err(error)=save_raster(path,&pixels,width){failures.push(format!("{phase}: {error:#}"));}
            let background=background_rgb(&pixels,width);
            if let Err(error)=verify_background(&pixels,width,dark,require_ink){failures.push(format!("{phase}: {error:#}"));}
            Some(Raster{width,pixels,background})
        },
        Err(error)=>{failures.push(format!("{phase}: {error:#}"));None},
    }
}
fn visible_selection_delta(selected:&[u8],plain:&[u8],width:usize)->(usize,usize){
    if selected.len()!=plain.len(){return (0,0);}
    let mut count=0;let mut min_x=width;let mut max_x=0;
    for (index,(a,b)) in selected.chunks_exact(4).zip(plain.chunks_exact(4)).enumerate(){
        if a[..3].iter().zip(&b[..3]).any(|(a,b)|a.abs_diff(*b)>=12){count+=1;min_x=min_x.min(index%width);max_x=max_x.max(index%width);}
    }
    (count,if count==0{0}else{max_x-min_x+1})
}
fn view_order(folder:&IFolderView2)->windows::core::Result<Vec<PathBuf>>{
    let order=windows::Win32::UI::Shell::_SVGIO(windows::Win32::UI::Shell::SVGIO_ALLVIEW.0|windows::Win32::UI::Shell::SVGIO_FLAG_VIEWORDER.0);
    let items:windows::Win32::UI::Shell::IShellItemArray=unsafe{folder.Items(order)}?;
    let mut paths=Vec::new();for index in 0..unsafe{items.GetCount()}?{if let Some(path)=search_item_path(&unsafe{items.GetItemAt(index)}?)?{paths.push(path);}}Ok(paths)
}
fn sort_observation(folder:&IFolderView2)->serde_json::Value{
    let count=unsafe{folder.GetSortColumnCount()}.unwrap_or(0);let mut columns=vec![windows::Win32::UI::Shell::SORTCOLUMN::default();count.max(0)as usize];
    let _=unsafe{folder.GetSortColumns(&mut columns)};
    serde_json::json!({"paths":view_order(folder).map(|paths|paths.iter().map(|path|path.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect::<Vec<_>>()).ok(),"sort":columns.iter().map(|column|serde_json::json!({"fmtid":format!("{:?}",column.propkey.fmtid),"pid":column.propkey.pid,"direction":column.direction.0})).collect::<Vec<_>>()})
}

fn exercise_results_palette(output:PathBuf,vista_layout:bool)->anyhow::Result<serde_json::Value>{
    ensure!(!crate::theme::high_contrast(),"dark/light raster fixture requires normal contrast; no system settings are changed");
    let controls=super::native_theme_test_context::CommonControls::activate(Some(&output))?;
    let process_theme=ProcessTheme::capture()?;
    let row_paths=[output.join("results-theme-A.current.txt"),output.join("results-theme-B.current.txt")];
    struct Rows(Vec<PathBuf>);impl Drop for Rows{fn drop(&mut self){
        use std::os::windows::fs::MetadataExt;
        for path in &self.0{if fs::symlink_metadata(path).is_ok_and(|meta|meta.file_attributes()&0x400==0)&&fs::read(path).is_ok_and(|bytes|bytes==b"results palette fixture"){let _=fs::remove_file(path);}}
    }}let mut rows=Rows(Vec::new());
    for path in &row_paths{let mut row=fs::OpenOptions::new().write(true).create_new(true).open(path)?;rows.0.push(path.clone());row.write_all(b"results palette fixture")?;}
    unsafe{OleInitialize(None)}?;
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);let name=windows::core::w!("ZSDesk.ResultsPalette.Regression");
    unsafe{RegisterClassW(&WNDCLASSW{lpfnWndProc:Some(window_proc),hInstance:instance,lpszClassName:name,..Default::default()});}
    let window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,name,None,WS_POPUP|WS_CLIPCHILDREN,40,40,640,380,None,None,Some(instance),None)}?;
    struct Window(HWND);impl Drop for Window{fn drop(&mut self){
        // The browser has already unadvised and released on both success and
        // error paths. Reclaim any owned completion payload still queued.
        let mut message=MSG::default();while unsafe{PeekMessageW(&mut message,Some(self.0),WM_SHELL_NAVIGATED,WM_SHELL_SELECTION_CHANGED,PM_REMOVE)}.as_bool(){unsafe{DispatchMessageW(&message);}}
        let _=unsafe{DestroyWindow(self.0)};
    }}let window=Window(window);
    unsafe{let _=ShowWindow(window.0,SW_SHOWNOACTIVATE);}
    let foreground=unsafe{GetForegroundWindow()};let mut cases=Vec::new();let mut visual_failures=Vec::new();
    for (host_index,first_dark) in [true,false,true].into_iter().enumerate(){
        scheme(first_dark);
        crate::theme::allow_dark_for_window(window.0,first_dark);
        crate::theme::set_dark_frame(window.0,first_dark);
        let after_scheme=process_theme.observed_dark();
        let host=unsafe{ExplorerBrowserHost::create_transient(window.0,RECT{left:0,top:0,right:640,bottom:380},false,Panes::default())}?;
        if vista_layout{
            let options:windows::Win32::UI::Shell::IFolderViewOptions=host.browser.cast()?;
            unsafe{options.SetFolderViewOptions(windows::Win32::UI::Shell::FVO_VISTALAYOUT,windows::Win32::UI::Shell::FVO_VISTALAYOUT)}?;
        }
        let view_options=host.browser.cast::<windows::Win32::UI::Shell::IFolderViewOptions>().ok().and_then(|options|unsafe{options.GetFolderViewOptions()}.ok()).map(|options|options.0);
        let after_create=process_theme.observed_dark();
        let first_color=if first_dark{0x191919}else{0xfafafa};host.set_content_background(first_dark,first_color);
        unsafe{host.navigate(&ShellLocation::Search{query:"palette fixture".into(),scope:None})}?;
        let after_fill=process_theme.observed_dark();
        println!("{}",serde_json::json!({"phase":"results-theme-observation","requested_dark":first_dark,"after_scheme":after_scheme,"after_create":after_create,"after_fill":after_fill,"os_build":crate::theme::os_build(),"common_controls_version":controls.version(),"browser_container":host.theme_container.map(class),"container_theme":host.container_theme.get(),"vista_layout_requested":vista_layout,"view_options_readback":view_options,"parent_dwm_dark_frame_requested":first_dark}));
        for index in 0..2{
            let dark=first_dark;
            let color=if dark{0x191919}else{0xfafafa};
            if index>0{scheme(dark);host.set_content_background(dark,color);host.begin_search_results()?;}
            let expected=view_background::Colors{background:color,high_contrast:crate::theme::high_contrast(),dark};
            let deadline=Instant::now()+Duration::from_secs(8);
            let observed=loop{
                pump(15);
                if let Ok(view)=unsafe{host.current_view()}{
                    if view.cast::<IFolderView2>().and_then(|folder|unsafe{folder.GetFolder::<IResultsFolder>()}).is_ok(){
                        let hwnd=unsafe{view.GetWindow()}?;let renderers=renderers(hwnd);
                        if !host.navigation_pending.get()&&!renderers.is_empty()&&renderers.iter().all(|(hwnd,_,colors)|*colors==Some(expected)&&view_background::applied_theme(*hwnd)==Some((expected.dark,expected.high_contrast))){break renderers;}
                    }
                }
                ensure!(Instant::now()<deadline,"native search renderer did not receive the current palette: first_dark={first_dark} dark={dark} reuse={index}");
            };
            for (hwnd,kind,_) in &observed{if kind=="SysListView32"{
                let background=unsafe{SendMessageW(*hwnd,LVM_GETBKCOLOR,None,None)}.0 as u32;
                let text=unsafe{SendMessageW(*hwnd,LVM_GETTEXTCOLOR,None,None)}.0 as u32;
                if !expected.high_contrast{ensure!(background==color&&text==if dark{0xffffff}else{0},"native results list has mismatched actual colors");}
            }}
            let initial_view=unsafe{host.current_view()}?;let initial_window=unsafe{initial_view.GetWindow()}?;
            unsafe{initial_view.UIActivate(windows::Win32::UI::Shell::SVUIA_ACTIVATE_FOCUS.0 as u32)}?;pump(30);
            let initial_folder:IFolderView2=initial_view.cast()?;let clear_deadline=Instant::now()+Duration::from_secs(3);
            while unsafe{initial_folder.ItemCount(windows::Win32::UI::Shell::SVGIO_ALLVIEW)}.unwrap_or(-1)!=0{ensure!(Instant::now()<clear_deadline,"previous rows did not finish clearing");pump(10);}
            println!("{}",serde_json::json!({"phase":"results-before-append","host":host_index,"query":index,"view":sort_observation(&initial_folder)}));
            println!("{}",serde_json::json!({"phase":"before-results-raster","requested_dark":dark,"should_apps_use_dark":process_theme.observed_dark(),"children":child_tree(initial_window)}));
            let initial=diagnostic_raster(initial_window,&output.join(if dark{"results-dark-initial.current.bmp"}else{"results-light-initial.current.bmp"}),dark,false,&format!("host {host_index} query {index} empty"),&mut visual_failures);
            let insertion=if index==0{row_paths.clone()}else{[row_paths[1].clone(),row_paths[0].clone()]};
            ensure!(host.append_search_paths(&insertion)?==2,"fixture results were not added");
            let view=unsafe{host.current_view()}?;let folder:IFolderView2=view.cast()?;
            let deadline=Instant::now()+Duration::from_secs(3);
            while unsafe{folder.ItemCount(windows::Win32::UI::Shell::SVGIO_ALLVIEW)}.unwrap_or(0)!=2{ensure!(Instant::now()<deadline,"fixture results did not materialize");pump(10);}
            if index>0{ensure!(view_order(&folder)?==insertion,"reused view changed the backend-sorted page order");}
            let mut name_key=PROPERTYKEY::default();unsafe{windows::Win32::UI::Shell::PropertiesSystem::PSGetPropertyKeyFromName(windows::core::w!("System.ItemNameDisplay"),&mut name_key)}?;
            // Change the requested order as the column-header action does.
            // Streamed pages already arrive in the requested backend order.
            // A reused view is fed that order before exercising both explicit
            // sort directions, rather than relying on an unchanged descriptor.
            let ascending=windows::Win32::UI::Shell::SORTCOLUMN{propkey:name_key,direction:windows::Win32::UI::Shell::SORT_ASCENDING};
            unsafe{folder.SetSortColumns(&[ascending])}?;pump(30);
            let ascending_deadline=Instant::now()+Duration::from_secs(3);
            while view_order(&folder)?!=row_paths{ensure!(Instant::now()<ascending_deadline,"result rows did not honor ascending name sorting");pump(10);}
            let descending=windows::Win32::UI::Shell::SORTCOLUMN{propkey:name_key,direction:windows::Win32::UI::Shell::SORT_DESCENDING};
            unsafe{folder.SetSortColumns(&[descending])}?;
            println!("{}",serde_json::json!({"phase":"results-after-sort","host":host_index,"query":index,"view":sort_observation(&folder)}));
            let deadline=Instant::now()+Duration::from_secs(3);
            loop{
                let order=windows::Win32::UI::Shell::_SVGIO(windows::Win32::UI::Shell::SVGIO_ALLVIEW.0|windows::Win32::UI::Shell::SVGIO_FLAG_VIEWORDER.0);
                let items:windows::Win32::UI::Shell::IShellItemArray=unsafe{folder.Items(order)}?;
                if unsafe{items.GetCount()}?==2&&search_item_path(&unsafe{items.GetItemAt(0)}?)?.as_ref()==Some(&row_paths[1]){break;}
                ensure!(Instant::now()<deadline,"result rows did not honor descending name sorting: {}",sort_observation(&folder));pump(10);
            }
            let flags=(windows::Win32::UI::Shell::FWF_FULLROWSELECT|windows::Win32::UI::Shell::FWF_SHOWSELALWAYS).0 as u32;
            unsafe{folder.SetCurrentFolderFlags(flags,flags)}?;
            let selected=(windows::Win32::UI::Shell::SVSI_SELECT.0|windows::Win32::UI::Shell::SVSI_FOCUSED.0|windows::Win32::UI::Shell::SVSI_NOTAKEFOCUS.0)as u32;
            unsafe{folder.SelectItem(0,selected)}?;pump(50);
            ensure!(unsafe{folder.ItemCount(windows::Win32::UI::Shell::SVGIO_SELECTION)}?==1,"fixture selection was not set");
            let selected_items:windows::Win32::UI::Shell::IShellItemArray=unsafe{folder.Items(windows::Win32::UI::Shell::SVGIO_SELECTION)}?;
            ensure!(search_item_path(&unsafe{selected_items.GetItemAt(0)}?)?.as_ref()==Some(&row_paths[1]),"sorted selection targeted a different result");
            let menu:windows::Win32::UI::Shell::IContextMenu=unsafe{view.GetItemObject(windows::Win32::UI::Shell::SVGIO_SELECTION)}?;drop(menu);
            let identity=view.cast::<IUnknown>()?;
            ensure!(!configure_results_columns(&view)&&!configure_results_columns(&view),"unchanged result columns or mode were reset");
            ensure!(unsafe{host.current_view()}?.cast::<IUnknown>()?==identity&&unsafe{folder.ItemCount(windows::Win32::UI::Shell::SVGIO_SELECTION)}?==1,"idempotent result layout lost view or selection");
            // Exercise palette changes while OnViewCreated is outstanding.
            // Desired colors may change, but the renderer is not rethemed until
            // the completion callback verifies the current ResultsFolder.
            let view_window=unsafe{view.GetWindow()}?;
            if index==1{
            unsafe{host.events.OnViewCreated(&view)}?;
            host.set_content_background(!dark,if dark{0xfafafa}else{0x191919});
            ensure!(host.navigation_pending.get(),"view creation did not defer theme application");
            ensure!(renderers(view_window).iter().all(|(hwnd,_,_)|view_background::applied_theme(*hwnd).is_none()),"pending view was rethemed");
            ensure!(unsafe{folder.ItemCount(windows::Win32::UI::Shell::SVGIO_SELECTION)}?==1,"pending palette update lost selection");
            // A final same-scheme update supersedes the temporary one before
            // completion. Actual scheme changes use a freshly created browser,
            // as the application does when rebuilding its native views.
            host.set_content_background(dark,color);
            unsafe{host.events.OnNavigationComplete(std::ptr::null())}?;pump(80);
            ensure!(!host.navigation_pending.get()&&unsafe{folder.ItemCount(windows::Win32::UI::Shell::SVGIO_SELECTION)}?==1,"completed palette update lost selection");
            }
            println!("{}",serde_json::json!({"phase":"selected-results-raster","requested_dark":dark,"children":child_tree(view_window)}));
            let selected_raster=diagnostic_raster(view_window,&output.join(if dark{"results-dark-selected.current.bmp"}else{"results-light-selected.current.bmp"}),dark,true,&format!("host {host_index} query {index} selected rows"),&mut visual_failures);
            unsafe{view.SelectItem(std::ptr::null(),windows::Win32::UI::Shell::SVSI_DESELECTOTHERS.0 as u32)}?;pump(50);
            let plain=diagnostic_raster(view_window,&output.join(if dark{"results-dark-plain.current.bmp"}else{"results-light-plain.current.bmp"}),dark,true,&format!("host {host_index} query {index} plain rows"),&mut visual_failures);
            let delta=selected_raster.as_ref().zip(plain.as_ref()).and_then(|(selected,plain)|{
                if selected.width!=plain.width{visual_failures.push(format!("host {host_index} query {index}: renderer geometry changed"));return None;}
                let (changed,span)=visible_selection_delta(&selected.pixels,&plain.pixels,selected.width);
                if changed<80||span<selected.width/3{visual_failures.push(format!("host {host_index} query {index}: selection not visually distinct, pixels={changed}, span={span}"));}
                Some((changed,span))
            });
            cases.push(serde_json::json!({"dark":dark,"host":host_index,"browser_recreated_for_scheme_change":host_index>0,"reused_query":index>0,"renderer_classes":observed.iter().map(|(_,kind,_)|kind).collect::<Vec<_>>(),"palette_attached_to_actual_renderers":true,"deferred_temporary_palette_preserves_selection":index==1,"unchanged_layout_preserves_view_and_selection":true,"descending_sort_and_selection":true,"selected_menu_source_available":true,"selection_pixel_delta":delta,"initial_background_rgb":initial.as_ref().map(|r|r.background),"selected_background_rgb":selected_raster.as_ref().map(|r|r.background),"plain_background_rgb":plain.as_ref().map(|r|r.background)}));
        }
        drop(host);pump(30);
    }
    let foreground_after=unsafe{GetForegroundWindow()};ensure!(foreground_after!=window.0,"hidden results fixture became foreground");
    drop(window);pump(15);
    for path in &row_paths{ensure!(fs::read(path)?==b"results palette fixture","fixture file changed");}drop(rows);ensure!(row_paths.iter().all(|path|!path.exists()),"fixture file cleanup failed");
    let previous_dark=process_theme.previous_dark;drop(process_theme);ensure!(crate::theme::menus_dark()==previous_dark,"test process menu color state was not restored");
    let controls_version=controls.version();drop(controls);
    Ok(serde_json::json!({"state":if visual_failures.is_empty(){"passed"}else{"failed"},"visual_failures":visual_failures,"cases":cases,"fixture_not_foreground":true,"foreground_same_window":foreground==foreground_after,"native_view_state_not_persisted":true,"process_color_scheme_restored":true,"common_controls_version":controls_version,"vista_layout_requested":vista_layout}))
}

#[test]
#[ignore="Actual ResultsFolder lifecycle on an unswitched private desktop; explicit fixture output and dedicated process"]
fn native_results_renderer_receives_palette_after_fill_and_reuse()->anyhow::Result<()>{
    let arguments=std::env::args().skip(1).collect::<Vec<_>>();
    ensure!(arguments.iter().filter(|value|!value.starts_with('-')).count()==1
        &&arguments.iter().any(|value|value.ends_with("native_results_renderer_receives_palette_after_fill_and_reuse"))
        &&arguments.iter().any(|value|value=="--exact")&&arguments.iter().any(|value|value=="--test-threads=1"),
        "run the ResultsFolder palette fixture alone with its exact filter and --test-threads=1");
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("explicit fixture output")?);
    ensure!(output.is_absolute()&&output.parent().is_some_and(|parent|parent.parent().is_some()),"dedicated absolute fixture output required");
    for ancestor in output.ancestors(){if ancestor.exists(){use std::os::windows::fs::MetadataExt;ensure!(fs::symlink_metadata(ancestor)?.file_attributes()&0x400==0,"fixture output must use real directories");}}
    fs::create_dir_all(&output)?;let report_path=output.join("results-palette.current.json");
    if report_path.exists(){use std::os::windows::fs::MetadataExt;ensure!(fs::symlink_metadata(&report_path)?.file_attributes()&0x400==0,"report target is a reparse point");}
    fs::write(&report_path,serde_json::to_vec_pretty(&serde_json::json!({"state":"running","pid":std::process::id()}))?)?;
    let target=output.clone();let vista_layout=std::env::var("ZSDESK_TEST_RESULTS_VISTA_LAYOUT").is_ok_and(|value|value=="1");
    let result=super::native_theme_test_context::private_desktop("results",move||exercise_results_palette(target,vista_layout));
    let report=match &result{Ok(report)=>{let mut report=report.clone();report["private_desktop_closed"]=serde_json::json!(true);report},Err(error)=>serde_json::json!({"state":"failed","error":format!("{error:#}"),"vista_layout_requested":vista_layout})};
    if report_path.exists(){use std::os::windows::fs::MetadataExt;ensure!(fs::symlink_metadata(&report_path)?.file_attributes()&0x400==0,"report target is a reparse point");}
    fs::write(report_path,serde_json::to_vec_pretty(&report)?)?;println!("{report}");
    result.and_then(|report|{ensure!(report["state"]=="passed","native ResultsFolder visual checks failed: {}",report["visual_failures"]);Ok(())})
}
