//! Raster checks for opaque tab strips; no foreground window or user input.
use super::*;
use anyhow::{Context,ensure};
use std::{fs,path::Path};
use zsdesk_ui::chrome::TabSlot;

fn pixel(surface:&Surface,x:i32,y:i32)->[u8;4]{
    assert!(x>=0&&y>=0&&x<surface.width&&y<surface.height);
    let offset=((y*surface.width+x)*4)as usize;
    let bytes=unsafe{std::slice::from_raw_parts(surface.bits.add(offset),4)};
    [bytes[2],bytes[1],bytes[0],bytes[3]]
}
fn close(actual:[u8;4],expected:zsdesk_ui::theme::Color)->bool{
    actual.into_iter().zip([expected.r,expected.g,expected.b,expected.a]).all(|(a,b)|a.abs_diff(b)<=3)
}
fn luminance(color:[u8;4])->f64{
    let linear=|v:u8|{let v=f64::from(v)/255.0;if v<=0.04045{v/12.92}else{((v+0.055)/1.055).powf(2.4)}};
    0.2126*linear(color[0])+0.7152*linear(color[1])+0.0722*linear(color[2])
}
fn contrast(a:[u8;4],b:[u8;4])->f64{let(a,b)=(luminance(a),luminance(b));(a.max(b)+0.05)/(a.min(b)+0.05)}
fn save_bmp(path:&Path,width:i32,height:i32,pixels:&[u8])->Result<()>{
    ensure!(path.is_absolute()&&pixels.len()==(width*height*4)as usize,"invalid raster output");
    if path.exists(){use std::os::windows::fs::MetadataExt;ensure!(fs::symlink_metadata(path)?.file_attributes()&0x400==0,"raster target is a reparse point");}
    let mut data=Vec::with_capacity(54+pixels.len());data.extend(b"BM");data.extend((54u32+pixels.len()as u32).to_le_bytes());data.extend([0;4]);data.extend(54u32.to_le_bytes());
    data.extend(40u32.to_le_bytes());data.extend(width.to_le_bytes());data.extend((-height).to_le_bytes());data.extend(1u16.to_le_bytes());data.extend(32u16.to_le_bytes());
    data.extend([0;4]);data.extend((pixels.len()as u32).to_le_bytes());data.extend([0;16]);data.extend(pixels);fs::write(path,data)?;Ok(())
}

#[test]
fn opaque_tabs_keep_selected_background_and_readable_text_without_accent_marker()->Result<()>{
    unsafe{OleInitialize(None)}?;
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let window=unsafe{CreateWindowExW(WS_EX_NOACTIVATE,windows::core::w!("STATIC"),None,WS_POPUP,-30000,-30000,480,240,None,None,None,None)}?;
    struct Window(HWND);impl Drop for Window{fn drop(&mut self){let _=unsafe{DestroyWindow(self.0)};}}let _window=Window(window);
    unsafe{SetPropW(window,windows::core::w!("ZSDesk.NativeCaption"),Some(windows::Win32::Foundation::HANDLE(1usize as*mut _)))}?;
    let foreground=unsafe{GetForegroundWindow()};
    let gfx=Gfx::new("Segoe UI")?;
    let mut session=storage::default_session();session.tabs=vec![ExplorerTab::new(1,ShellLocation::Path(r"C:\Selected tab".into())),ExplorerTab::new(2,ShellLocation::Path(r"C:\Other tab".into()))];session.active_tab=0;
    let mut state=WindowState::new(session,std::env::temp_dir().join("zsdesk-owned-tab-raster/explorer-session.json"),None,false);
    state.hwnd=window;state.mica=false;state.dpi=96;state.hover=Hit::None;
    let mut raster_paths=Vec::new();
    for vertical in [false,true]{
        state.layout=ChromeLayout::default();state.layout.strip=LayoutRect::new(0,0,480,44);state.layout.vertical_tabs=vertical;
        if vertical{state.layout.tab_panel=LayoutRect::new(0,44,220,196);state.layout.nav_row=LayoutRect::new(220,44,260,48);}
        let rects=if vertical{[LayoutRect::new(8,50,204,34),LayoutRect::new(8,92,204,34)]}else{[LayoutRect::new(16,6,180,34),LayoutRect::new(208,6,180,34)]};
        state.layout.tabs=rects.into_iter().enumerate().map(|(index,rect)|TabSlot{index,rect,close:LayoutRect::default(),show_close:false}).collect();
        let mut mosaic=vec![0u8;960*480*4];
        for (case,(dark,active)) in [(false,true),(false,false),(true,true),(true,false)].into_iter().enumerate(){
          for neutral_accent in [false,true]{
            state.palette=if dark{Palette::dark()}else{Palette::light()};state.dark=dark;state.active=active;
            if neutral_accent{state.palette.accent=state.palette.layer;}
            let surface=Surface::new(480,240).context("tab raster surface")?;
            gfx.draw(&surface,|canvas|state.paint_strip(canvas,&[None,None]))?;
            let selected=state.layout.tabs[0].rect;let other=state.layout.tabs[1].rect;
            let selected_pixel=pixel(&surface,selected.x+50,selected.y+4);let other_pixel=pixel(&surface,other.x+50,other.y+4);
            ensure!(close(selected_pixel,state.palette.layer)&&close(other_pixel,state.palette.strip),"opaque tab backgrounds lost selection: dark={dark} active={active} vertical={vertical}, selected={selected_pixel:?}, other={other_pixel:?}");
            ensure!(selected_pixel!=other_pixel,"selected tab is indistinguishable from the strip");
            let edge=if vertical{pixel(&surface,selected.x+3,selected.y+selected.height/2)}else{pixel(&surface,selected.x+selected.width/2,selected.bottom()-2)};
            ensure!(close(edge,state.palette.layer),"unexpected accent decoration on selected tab: dark={dark} active={active} vertical={vertical}: {edge:?}");
            let background=[state.palette.layer.r,state.palette.layer.g,state.palette.layer.b,255];let mut readable=0;
            for y in selected.y+9..selected.bottom()-7{for x in selected.x+38..selected.right()-8{if contrast(pixel(&surface,x,y),background)>=3.0{readable+=1;}}}
            ensure!(readable>=20,"selected label has insufficient rendered contrast: dark={dark} active={active} vertical={vertical}");
            if !neutral_accent{
                let bytes=unsafe{std::slice::from_raw_parts(surface.bits,(surface.width*surface.height*4)as usize)};
                let tile_x=(case%2)*480;let tile_y=(case/2)*240;
                for y in 0..240usize{let target=((tile_y+y)*960+tile_x)*4;mosaic[target..target+480*4].copy_from_slice(&bytes[y*480*4..(y+1)*480*4]);}
            }
          }
        }
        if let Some(output)=std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT"){
            let output=PathBuf::from(output);ensure!(output.is_absolute()&&output.parent().is_some_and(|parent|parent.parent().is_some()),"dedicated absolute raster output required");
            for ancestor in output.ancestors(){if ancestor.exists(){use std::os::windows::fs::MetadataExt;ensure!(fs::symlink_metadata(ancestor)?.file_attributes()&0x400==0,"raster output crosses a reparse point");}}
            fs::create_dir_all(&output)?;let path=output.join(if vertical{"tab-contrast-vertical.current.bmp"}else{"tab-contrast-horizontal.current.bmp"});save_bmp(&path,960,480,&mosaic)?;raster_paths.push(path);
        }
    }
    let after=unsafe{GetForegroundWindow()};ensure!(after!=window,"tab raster fixture became foreground");
    println!("{}",serde_json::json!({"modes":["light","dark"],"focused_and_unfocused":true,"top_and_side_tabs":true,"mica":false,"selected_background_distinct":true,"no_accent_marker":true,"readable_label_pixels":true,"fixture_not_foreground":true,"foreground_same_window":foreground==after,"rasters":raster_paths}));
    Ok(())
}
