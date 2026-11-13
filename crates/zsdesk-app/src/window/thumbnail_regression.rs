//! Offscreen native Shell icon rendering; no input or activation is performed.
use super::*;
use std::{fs,path::Path,thread,time::{Duration,Instant}};
use anyhow::ensure;
use windows::{core::Interface,Win32::{Foundation::GENERIC_WRITE,System::Com::{CoCreateInstance,CLSCTX_INPROC_SERVER},
    Graphics::Imaging::{IWICImagingFactory,CLSID_WICImagingFactory,GUID_ContainerFormatPng,GUID_WICPixelFormat32bppBGRA,WICBitmapEncoderNoCache},
    UI::Shell::{IFolderView2,IShellView,FVM_ICON,FVM_DETAILS,SVGIO_ALLVIEW,FWF_NOCOLUMNHEADER,FWF_NOHEADERINALLVIEWS,FWF_CHECKSELECT}}};

#[link(name="user32")]unsafe extern "system"{fn PrintWindow(window:HWND,dc:HDC,flags:u32)->i32;}
struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}
struct Owner(HWND);impl Drop for Owner{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}
struct Scheme;impl Drop for Scheme{fn drop(&mut self){theme::configure_color_scheme(zsdesk_core::model::explorer::ColorScheme::System);}}
fn pump(ms:u64){
    let end=Instant::now()+Duration::from_millis(ms);
    while Instant::now()<end{
        for _ in 0..512{let mut message=MSG::default();if !unsafe{PeekMessageW(&mut message,None,0,0,PM_REMOVE)}.as_bool(){break;}
            if message.message!=WM_QUIT{unsafe{let _=TranslateMessage(&message);DispatchMessageW(&message);}}}
        thread::sleep(Duration::from_millis(3));
    }
}
fn png(factory:&IWICImagingFactory,path:&Path,width:u32,height:u32,pixels:&[u8])->Result<()>{
    let stream=unsafe{factory.CreateStream()}?;let name=wide(&path.to_string_lossy());
    unsafe{stream.InitializeFromFilename(PCWSTR(name.as_ptr()),GENERIC_WRITE.0)}?;
    let encoder=unsafe{factory.CreateEncoder(&GUID_ContainerFormatPng,std::ptr::null())}?;
    unsafe{encoder.Initialize(&stream,WICBitmapEncoderNoCache)}?;
    let mut frame=None;let mut options=None;unsafe{encoder.CreateNewFrame(&mut frame,&mut options)}?;
    let frame=frame.context("PNG frame")?;unsafe{frame.Initialize(options.as_ref())?;frame.SetSize(width,height)?;}
    let mut format=GUID_WICPixelFormat32bppBGRA;unsafe{frame.SetPixelFormat(&mut format)}?;
    ensure!(format==GUID_WICPixelFormat32bppBGRA,"PNG encoder changed pixel layout");
    unsafe{frame.WritePixels(height,width*4,pixels)?;frame.Commit()?;encoder.Commit()?;}Ok(())
}
fn fixtures(factory:&IWICImagingFactory,path:&Path)->Result<()>{
    fs::create_dir_all(path)?;
    let mut solid=vec![0u8;256*256*4];let mut transparent=solid.clone();
    for y in 0..256{for x in 0..256{let at=(y*256+x)*4;
        solid[at..at+4].copy_from_slice(&[220,180,25,255]);
        if (64..192).contains(&x)&&(64..192).contains(&y){transparent[at..at+4].copy_from_slice(&[80,200,30,255]);}
    }}
    png(factory,&path.join("01-cyan.png"),256,256,&solid)?;
    png(factory,&path.join("02-transparent-green.png"),256,256,&transparent)?;
    for name in ["03-folder-cyan","04-folder-green"]{fs::create_dir_all(path.join(name))?;}
    png(factory,&path.join("03-folder-cyan/image.png"),256,256,&solid)?;
    png(factory,&path.join("04-folder-green/image.png"),256,256,&transparent)?;
    Ok(())
}
fn capture(shell:&IShellView)->Result<(u32,u32,Vec<u8>)>{
    let window=unsafe{shell.GetWindow()}?;let mut rect=RECT::default();unsafe{GetClientRect(window,&mut rect)}?;
    ensure!(rect.right>100&&rect.bottom>100&&i64::from(rect.right)*i64::from(rect.bottom)<4_000_000,"invalid capture dimensions");
    let surface=Surface::new(rect.right,rect.bottom).context("capture surface")?;
    let pixels=unsafe{std::slice::from_raw_parts_mut(surface.bits,(rect.right*rect.bottom*4)as usize)};pixels.fill(127);
    ensure!(unsafe{PrintWindow(window,surface.dc,2)}!=0,"PrintWindow failed");
    for pixel in pixels.chunks_exact_mut(4){pixel[3]=255;}
    Ok((rect.right as u32,rect.bottom as u32,pixels.to_vec()))
}
fn markers(pixels:&[u8])->(usize,usize){
    let mut cyan=0;let mut green=0;
    for pixel in pixels.chunks_exact(4){
        if pixel[0]>170&&pixel[1]>120&&pixel[2]<70{cyan+=1;}
        if (45..=110).contains(&pixel[0])&&pixel[1]>150&&pixel[2]<75{green+=1;}
    }(cyan,green)
}
fn black_components(pixels:&[u8],width:usize,height:usize,icon:usize)->Vec<[usize;5]>{
    let mut black:Vec<bool>=pixels.chunks_exact(4).map(|pixel|pixel[..3].iter().all(|channel|*channel<=2)).collect();
    let mut result=Vec::new();let mut stack=Vec::new();
    for start in 0..black.len(){if !black[start]{continue;}black[start]=false;stack.push(start);
        let(mut count,mut left,mut top,mut right,mut bottom)=(0,usize::MAX,usize::MAX,0,0);
        while let Some(at)=stack.pop(){let x=at%width;let y=at/width;count+=1;left=left.min(x);right=right.max(x);top=top.min(y);bottom=bottom.max(y);
            for next in [x.checked_sub(1).map(|_|at-1),(x+1<width).then_some(at+1),y.checked_sub(1).map(|_|at-width),(y+1<height).then_some(at+width)].into_iter().flatten(){
                if black[next]{black[next]=false;stack.push(next);}
            }
        }
        if count>=icon*icon/8&&right-left+1>=icon/3&&bottom-top+1>=icon/3{result.push([count,left,top,right,bottom]);}
    }result
}
fn frame(factory:&IWICImagingFactory,output:&Path,folder:&Path,dark:bool,icon:i32)->Result<serde_json::Value>{
    use zsdesk_core::model::explorer::{ColorScheme,BackdropStyle};
    let mut session=storage::default_session();session.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(folder.into()))];session.next_id=2;
    session.ui.appearance.color_scheme=if dark{ColorScheme::Dark}else{ColorScheme::Light};session.ui.appearance.backdrop=BackdropStyle::Solid;
    let slot=Box::new(RefCell::new(Some(WindowState::new(session,output.join(format!("profile-{dark}-{icon}/explorer-session.json")),None,false))));
    let instance=HINSTANCE(unsafe{GetModuleHandleW(None)}?.0);let class=windows::core::w!("ZSDesk.Native.ThumbnailRegression");
    unsafe{RegisterClassW(&WNDCLASSW{hInstance:instance,lpfnWndProc:Some(window_proc),lpszClassName:class,..Default::default()});}
    let screen_right=unsafe{GetSystemMetrics(SM_XVIRTUALSCREEN)+GetSystemMetrics(SM_CXVIRTUALSCREEN)};
    let screen_bottom=unsafe{GetSystemMetrics(SM_YVIRTUALSCREEN)+GetSystemMetrics(SM_CYVIRTUALSCREEN)};
    let owner=Owner(unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,class,windows::core::w!("Native icon rendering"),WS_OVERLAPPEDWINDOW|WS_CLIPCHILDREN,
        screen_right+1024,screen_bottom+1024,1480,1000,None,None,Some(instance),Some((&*slot as*const Slot).cast()))}?);
    unsafe{slot.borrow_mut().as_mut().unwrap().create(owner.0)?;let _=ShowWindow(owner.0,SW_SHOWNOACTIVATE);}
    let deadline=Instant::now()+Duration::from_secs(12);
    loop{pump(30);if slot.borrow().as_ref().is_some_and(|state|state.pending.is_none()&&state.shown_tab.is_some()){break;}
        ensure!(Instant::now()<deadline,"native thumbnail navigation timed out");}
    ensure!(slot.borrow().as_ref().unwrap().dark==dark,"run this regression with --features themed");
    let current=||->Result<IShellView>{Ok(unsafe{slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().current_view()}?)};
    let initial:IFolderView2=current()?.cast()?;let mut initial_mode=windows::Win32::UI::Shell::FOLDERVIEWMODE(0);let mut initial_icon=0;
    unsafe{initial.GetViewModeAndIconSize(&mut initial_mode,&mut initial_icon)}?;
    let initial_flags=unsafe{initial.GetCurrentFolderFlags()}?;
    ensure!(initial_flags&FWF_NOHEADERINALLVIEWS.0 as u32!=0&&(initial_flags&FWF_NOCOLUMNHEADER.0 as u32!=0)==(initial_mode!=FVM_DETAILS),"completed navigation did not apply header policy");
    drop(initial);
    {slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().set_checkboxes(true)?;}
    let set_mode=|mode:i32,size:i32|->Result<u32>{
        {slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().set_view_mode(mode,size)?;}
        pump(120);let view:IFolderView2=current()?.cast()?;let flags=unsafe{view.GetCurrentFolderFlags()}?;
        ensure!(flags&FWF_NOHEADERINALLVIEWS.0 as u32!=0&&(flags&FWF_NOCOLUMNHEADER.0 as u32!=0)==(mode!=FVM_DETAILS.0),"mode {mode} has incorrect column header flags");
        ensure!(flags&FWF_CHECKSELECT.0 as u32!=0,"header policy changed unrelated checkbox flags");Ok(flags)
    };
    let icon_flags=set_mode(FVM_ICON.0,icon)?;
    let detail_flags=set_mode(FVM_DETAILS.0,16)?;
    let(details_width,details_height,details_pixels)=capture(&current()?)?;
    let details_image=output.join(format!("{}-{icon}-details.png",if dark{"dark"}else{"light"}));png(factory,&details_image,details_width,details_height,&details_pixels)?;
    set_mode(FVM_ICON.0,icon)?;
    {slot.borrow().as_ref().unwrap().browser.as_ref().unwrap().set_checkboxes(false)?;}
    let shell=current()?;let view:IFolderView2=shell.cast()?;
    let deadline=Instant::now()+Duration::from_secs(15);
    let(width,height,pixels,cyan,green)=loop{
        pump(100);let(width,height,pixels)=capture(&shell)?;let(cyan,green)=markers(&pixels);
        if cyan>icon as usize*icon as usize/8&&green>icon as usize*icon as usize/32{break(width,height,pixels,cyan,green);}
        ensure!(Instant::now()<deadline,"actual PNG thumbnails did not render: cyan={cyan}, green={green}, dark={dark}, icon={icon}");
    };
    let count=unsafe{view.ItemCount(SVGIO_ALLVIEW)}?;ensure!(count==4,"native fixture item count changed");
    let black=black_components(&pixels,width as usize,height as usize,icon as usize);
    let mut colors=std::collections::HashMap::<[u8;3],usize>::new();
    for pixel in pixels.chunks_exact(4){*colors.entry([pixel[0],pixel[1],pixel[2]]).or_default()+=1;}
    let(background,background_pixels)=colors.into_iter().max_by_key(|(_,count)|*count).context("native background pixels")?;
    let image=output.join(format!("{}-{icon}.png",if dark{"dark"}else{"light"}));png(factory,&image,width,height,&pixels)?;
    let mut bounds=RECT::default();unsafe{GetWindowRect(owner.0,&mut bounds)}?;
    ensure!(bounds.left>screen_right&&bounds.top>screen_bottom,"test window entered the visible desktop");
    ensure!(background_pixels>(width*height/4)as usize&&background.iter().all(|color|if dark{*color<=80}else{*color>=200}),"native view did not adopt requested light/dark background: {background:?}");
    ensure!(black.is_empty(),"unexpected large black regions: {black:?}; image={}",image.display());
    drop(view);drop(shell);drop(owner);pump(40);
    Ok(serde_json::json!({"dark":dark,"icon_size":icon,"width":width,"height":height,"items":count,"cyan_pixels":cyan,"green_pixels":green,
        "large_black_components":black,"background_bgr":background,"background_pixels":background_pixels,"capture":image,"offscreen":true,"no_activate":true,
        "navigation_header_policy":true,"icon_header_hidden":icon_flags&FWF_NOCOLUMNHEADER.0 as u32!=0,
        "details_header_restored":detail_flags&FWF_NOCOLUMNHEADER.0 as u32==0,"unrelated_checkbox_flag_preserved":true,"details_capture":details_image}))
}

#[test]
#[ignore="Requires --features themed and ZSDESK_NATIVE_TEST_OUTPUT; offscreen native Shell views, no input or activation"]
fn native_large_icons_render_light_dark_without_black_placeholders()->Result<()>{
    ensure!(crate::edition::THEMED,"run with --features themed");ensure!(!theme::high_contrast(),"normal light/dark renderer required");
    let output=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output required")?).join("native-large-icons");
    ensure!(output.is_absolute(),"absolute output required");fs::create_dir_all(&output)?;
    let foreground=unsafe{GetForegroundWindow()};unsafe{OleInitialize(None)}?;let _apartment=Apartment;let _scheme=Scheme;
    let factory:IWICImagingFactory=unsafe{CoCreateInstance(&CLSID_WICImagingFactory,None,CLSCTX_INPROC_SERVER)}?;
    let folder=output.join("fixtures");fixtures(&factory,&folder)?;let mut frames=Vec::new();
    let result=(||->Result<()>{for dark in [false,true]{for icon in [128,256]{frames.push(frame(&factory,&output,&folder,dark,icon)?);}}Ok(())})();
    let current=unsafe{GetForegroundWindow()};let mut foreground_pid=0;unsafe{GetWindowThreadProcessId(current,Some(&mut foreground_pid));}
    let isolated=foreground_pid!=std::process::id();
    let report=serde_json::json!({"state":if result.is_ok()&&isolated{"passed"}else{"failed"},"frames":frames,
        "error":result.as_ref().err().map(|error|format!("{error:#}")),"input_sent":false,"windows_preferences_changed":false,
        "foreground_unchanged":foreground==current,"test_did_not_activate":isolated,
        "scope":"native IExplorerBrowser PNG and folder icons in themed 128/256 views; this is not a claim about untested third-party thumbnail providers"});
    fs::write(output.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
    result?;ensure!(isolated,"test window was activated");Ok(())
}
