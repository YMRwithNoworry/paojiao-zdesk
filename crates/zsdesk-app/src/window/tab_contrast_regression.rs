//! Production tab painting on memory surfaces; no windows, controls or input.
use super::*;
use anyhow::{Context,ensure};
use std::{fs,path::Path};

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

use zsdesk_core::model::explorer::TabPlacement;
fn render_tabs(gfx:&Gfx,palette:Palette,focused:bool,emphasis:bool,placement:TabPlacement,dpi:u32)->Result<Surface>{
    let vertical=placement!=TabPlacement::Top;
    let s=|value:f32|value*dpi as f32/96.0;
    let surface=Surface::new(s(480.0)as i32,s(144.0)as i32).context("memory tab surface")?;
    let selected=RectF::new(s(16.0),s(32.0),s(196.0),s(34.0));
    let other=if vertical{RectF::new(s(16.0),s(78.0),s(196.0),s(34.0))}else{RectF::new(s(232.0),s(32.0),s(196.0),s(34.0))};
    gfx.draw(&surface,|cv|{
        cv.fill(RectF::new(0.0,0.0,s(480.0),s(144.0)),c(palette.strip));
        for (r,active,label) in [(selected,true,"Current tab"),(other,false,"Another tab")]{
            let style=palette.tab_appearance(active,focused,emphasis);
            if active{paint_selected_tab(cv,r,placement,dpi,style,palette.accent);}
            cv.text(label,RectF::new(r.x+s(38.0),r.y,r.w-s(46.0),r.h),&TextStyle::ui(s(UI_TEXT)).weight(style.weight),c(style.text));
        }
    })?;Ok(surface)
}
fn pixels(surface:&Surface)->&[u8]{unsafe{std::slice::from_raw_parts(surface.bits,(surface.width*surface.height*4)as usize)}}

#[test]
fn command_row_background_survives_separate_paint_bands() -> Result<()> {
    unsafe { OleInitialize(None) }?;
    struct Apartment;
    impl Drop for Apartment { fn drop(&mut self) { unsafe { OleUninitialize() }; } }
    let _apartment = Apartment;
    let gfx = Gfx::new("Segoe UI")?;
    for dpi in [96, 120, 144, 192] {
        let scale = |v| v * dpi / 96;
        let layout = ChromeLayout {
            nav_row: LayoutRect::new(8, scale(40), 96, scale(48)),
            top: scale(176),
            ..Default::default()
        };
        for palette in [Palette::light(), Palette::dark()] {
            for translucent in [false, true] {
                let reference = Surface::new(112, layout.top).context("reference surface")?;
                let band = Surface::new(112, 96).context("band surface")?;
                let draw = |cv: &Canvas, top, bottom| paint_navigation_band(cv, &layout, top, bottom, c(palette.layer), translucent);
                gfx.draw(&reference, |cv| draw(cv, 0, layout.top))?;
                for top in (0..layout.top).step_by(96) {
                    gfx.draw_band(&band, top, |cv| draw(cv, top, top + 96))?;
                    for y in top.max(layout.nav_row.bottom())..layout.top.min(top + 96) {
                        let sample = pixel(&band, 16, y - top);
                        assert_eq!(sample, pixel(&reference, 16, y));
                        if translucent { assert!(sample[3] > 0); } else { assert!(close(sample, palette.layer)); }
                        assert_eq!(pixel(&band, 0, y - top), [0; 4]);
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn active_tab_marker_and_text_are_distinct_on_every_tab_placement()->Result<()>{
    unsafe{OleInitialize(None)}?;
    struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
    let gfx=Gfx::new("Segoe UI")?;let mut cases=0;let mut preview=vec![0u8;960*576*4];
    for placement in [TabPlacement::Top,TabPlacement::Left,TabPlacement::Right]{let vertical=placement!=TabPlacement::Top;for dpi in [96,144,192]{
        let s=|value:f32|value*dpi as f32/96.0;
        for (row,(dark,focused)) in [(false,true),(false,false),(true,true),(true,false)].into_iter().enumerate(){
            let palette=if dark{Palette::dark()}else{Palette::light()};
            let normal=render_tabs(&gfx,palette,focused,false,placement,dpi)?;
            let enhanced=render_tabs(&gfx,palette,focused,true,placement,dpi)?;
            ensure!(pixels(&normal)!=pixels(&enhanced),"extra outline toggle had no effect");
            let (mx,my)=match placement{TabPlacement::Top=>(114.0,64.5),TabPlacement::Left=>(20.5,49.0),TabPlacement::Right=>(207.5,49.0)};
            for image in [&normal,&enhanced]{ensure!(close(pixel(image,s(mx)as i32,s(my)as i32),palette.accent),"missing {placement:?} marker at {dpi}");}
            let fill=palette.tab_appearance(true,focused,true).fill.unwrap();let background=[fill.r,fill.g,fill.b,255];
            ensure!(close(pixel(&enhanced,s(66.0)as i32,s(37.0)as i32),fill),"enhanced selected fill is not opaque");
            let mut strongest=1.0f64;
            for x in s(16.0)as i32..s(20.0).ceil()as i32{strongest=strongest.max(contrast(pixel(&enhanced,x,s(49.0)as i32),background));}
            ensure!(strongest>=3.0,"selected outline lacks contrast: dark={dark} focused={focused} vertical={vertical} dpi={dpi}: {strongest}");
            let mut ink=0;
            for y in s(41.0)as i32..s(59.0)as i32{for x in s(54.0)as i32..s(195.0)as i32{if contrast(pixel(&enhanced,x,y),background)>=4.5{ink+=1;}}}
            ensure!(ink>=20,"selected text is too faint after window deactivation");
            if dpi==96&&!vertical{
                for(column,surface)in [&normal,&enhanced].into_iter().enumerate(){
                    for y in 0..144usize{let at=((row*144+y)*960+column*480)*4;preview[at..at+480*4].copy_from_slice(&pixels(surface)[y*480*4..(y+1)*480*4]);}
                }
            }
            cases+=1;
        }
    }}
    if let Some(output)=std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT"){
        let root=PathBuf::from(output);ensure!(root.is_absolute(),"absolute memory-raster directory required");
        for parent in root.ancestors(){if parent.exists(){use std::os::windows::fs::MetadataExt;ensure!(fs::symlink_metadata(parent)?.file_attributes()&0x400==0,"raster output crosses a reparse point");}}
        fs::create_dir_all(&root)?;save_bmp(&root.join("tab-emphasis.current.bmp"),960,576,&preview)?;
    }
    println!("{}",serde_json::json!({"cases":cases,"windowless":true,"all_tab_placements":true,"system_accent_marker":true,"selected_border_contrast":3.0,"selected_text_contrast":4.5}));
    Ok(())
}
