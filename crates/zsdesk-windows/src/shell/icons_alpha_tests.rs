//! Native GDI/WIC pixel contracts; no windows or desktop input are created.
use super::*;
use windows::Win32::{Graphics::Gdi::{CreateDIBSection,CreateBitmap},UI::Shell::SharedBitmap};

struct Bitmap(HBITMAP);
impl Drop for Bitmap{fn drop(&mut self){if !self.0.is_invalid(){let _=unsafe{DeleteObject(self.0.into())};}}}
struct Apartment;
impl Apartment{fn new()->windows::core::Result<Self>{unsafe{CoInitializeEx(None,COINIT_APARTMENTTHREADED)}.ok()?;Ok(Self)}}
impl Drop for Apartment{fn drop(&mut self){unsafe{CoUninitialize();}}}

fn dib(pixels:&[u8])->windows::core::Result<Bitmap>{
    assert!(!pixels.is_empty()&&pixels.len()%4==0);
    let info=BITMAPINFO{bmiHeader:BITMAPINFOHEADER{biSize:std::mem::size_of::<BITMAPINFOHEADER>()as u32,
        biWidth:(pixels.len()/4)as i32,biHeight:-1,biPlanes:1,biBitCount:32,biCompression:BI_RGB.0,..Default::default()},..Default::default()};
    let mut bits=std::ptr::null_mut();let bitmap=Bitmap(unsafe{CreateDIBSection(None,&info,DIB_RGB_COLORS,&mut bits,None,0)}?);
    assert!(!bits.is_null());unsafe{std::ptr::copy_nonoverlapping(pixels.as_ptr(),bits.cast::<u8>(),pixels.len());}
    Ok(bitmap)
}
fn wic_pixels(gfx:&crate::gfx::Gfx,bitmap:HBITMAP,alpha:BitmapAlpha)->Vec<u8>{
    let source=gfx.wic_from_hbitmap(bitmap,alpha).expect("WIC materialization");
    let(width,height)=crate::gfx::Gfx::wic_size(&source);let mut bytes=vec![0;4*(width*height)as usize];
    unsafe{source.CopyPixels(std::ptr::null(),width*4,&mut bytes)}.unwrap();bytes
}
fn composite(pixel:&[u8],background:u8)->[u8;3]{
    std::array::from_fn(|channel|(u32::from(pixel[channel])+(u32::from(background)*(255-u32::from(pixel[3]))+127)/255).min(255)as u8)
}
fn rendered(gfx:&crate::gfx::Gfx,bitmap:HBITMAP,alpha:BitmapAlpha,background:u8)->windows::core::Result<[u8;4]>{
    use crate::gfx::{Surface,RectF,Color};
    let source=gfx.wic_from_hbitmap(bitmap,alpha).expect("WIC render source");
    let uploaded=gfx.upload(&source).expect("software D2D bitmap");let surface=Surface::new(8,8).expect("memory surface");
    gfx.draw(&surface,|canvas|{
        canvas.fill(RectF::new(0.0,0.0,8.0,8.0),Color::rgba8(background,background,background,255));
        canvas.image(&uploaded,RectF::new(0.0,0.0,8.0,8.0),1.0);
    })?;
    Ok(unsafe{std::slice::from_raw_parts(surface.bits.add((4*8+4)*4),4)}.try_into().unwrap())
}

#[test]
fn shell_thumbnail_alpha_contract_preserves_transparency_and_real_black()->windows::core::Result<()>{
    let _apartment=Apartment::new()?;let gfx=crate::gfx::Gfx::new("Segoe UI")?;
    let cases=[
        ("transparent-black",WTSAT_ARGB,vec![0,0,0,0],vec![0,0,0,0]),
        ("opaque-black-without-alpha",WTSAT_RGB,vec![0,0,0,0],vec![0,0,0,255]),
        ("opaque-rgb-with-unused-high-byte",WTSAT_RGB,vec![10,20,30,64],vec![10,20,30,255]),
        ("premultiplied-half-alpha",WTSAT_ARGB,vec![64,0,0,128],vec![64,0,0,128]),
        ("straight-half-alpha",WTSAT_ARGB,vec![200,0,0,128],vec![100,0,0,128]),
        ("transparent-and-opaque-black",WTSAT_ARGB,vec![0,0,0,0,0,0,0,255],vec![0,0,0,0,0,0,0,255]),
    ];
    for (name,kind,pixels,expected) in cases{
        let mut bitmap=dib(&pixels)?;
        let shared:ISharedBitmap=unsafe{CoCreateInstance(&SharedBitmap,None,CLSCTX_INPROC_SERVER)}?;
        unsafe{shared.InitializeBitmap(bitmap.0,kind)}?;bitmap.0=HBITMAP::default();
        assert_eq!(unsafe{shared.GetFormat()}?,kind,"{name}");
        let(handle,alpha)=detach_thumbnail(&shared).expect("detach declared bitmap");let bitmap=Bitmap(handle);
        let actual=wic_pixels(&gfx,bitmap.0,alpha);
        assert_eq!(actual,expected,"{name}: GDI/WIC alpha contract changed the pixels");
        if actual.len()==4{for background in [255,32]{
            let rendered=rendered(&gfx,bitmap.0,alpha,background)?;
            assert_eq!(&rendered[..3],composite(&actual,background),"{name}: rendered color differs on background {background}");
            assert_eq!(rendered[3],255,"opaque backdrop lost opacity");
        }}
        if name=="transparent-black"{
            assert_eq!(composite(&actual,255),[255,255,255]);assert_eq!(composite(&actual,32),[32,32,32]);
        }else if name=="opaque-black-without-alpha"{
            assert_eq!(composite(&actual,255),[0,0,0]);assert_eq!(composite(&actual,32),[0,0,0]);
        }
    }
    Ok(())
}

#[test]
fn device_dependent_bitmap_keeps_declared_alpha_through_wic()->windows::core::Result<()>{
    let _apartment=Apartment::new()?;let gfx=crate::gfx::Gfx::new("Segoe UI")?;
    for(kind,pixels,expected)in [
        (WTSAT_ARGB,[0u8,0,0,0],[0u8,0,0,0]),
        (WTSAT_RGB,[0,0,0,0],[0,0,0,255]),
        (WTSAT_ARGB,[64,0,0,128],[64,0,0,128]),
        (WTSAT_UNKNOWN,[10,20,30,64],[10,20,30,255]),
    ]{
        let bitmap=Bitmap(unsafe{CreateBitmap(1,1,1,32,Some(pixels.as_ptr().cast()))});assert!(!bitmap.0.is_invalid());
        let mut section=DIBSECTION::default();let bytes=unsafe{GetObjectW(bitmap.0.into(),std::mem::size_of::<DIBSECTION>()as i32,Some((&mut section as*mut DIBSECTION).cast()))};
        assert_eq!(bytes as usize,std::mem::size_of::<BITMAP>(),"fixture is not a device-dependent bitmap");
        let actual=wic_pixels(&gfx,bitmap.0,classify_alpha(bitmap.0,kind));
        assert_eq!(actual,expected,"device-dependent alpha contract was lost");
    }
    let rgb=[0u8,0,0,80,40,20];
    let bitmap=Bitmap(unsafe{CreateBitmap(2,1,1,24,Some(rgb.as_ptr().cast()))});assert!(!bitmap.0.is_invalid());
    assert_eq!(wic_pixels(&gfx,bitmap.0,classify_alpha(bitmap.0,WTSAT_UNKNOWN)),[0,0,0,255,80,40,20,255],"24-bit black/color content became transparent");
    // DDB rows are word-aligned (four bytes here), unlike the three color bytes.
    let rgb=[0u8,0,0,201,80,40,20,202];
    let bitmap=Bitmap(unsafe{CreateBitmap(1,2,1,24,Some(rgb.as_ptr().cast()))});assert!(!bitmap.0.is_invalid());
    assert_eq!(wic_pixels(&gfx,bitmap.0,classify_alpha(bitmap.0,WTSAT_UNKNOWN)),[0,0,0,255,80,40,20,255],"24-bit DDB row padding/order was mishandled");
    let info=BITMAPINFO{bmiHeader:BITMAPINFOHEADER{biSize:std::mem::size_of::<BITMAPINFOHEADER>()as u32,
        biWidth:2,biHeight:-1,biPlanes:1,biBitCount:24,biCompression:BI_RGB.0,..Default::default()},..Default::default()};
    let mut bits=std::ptr::null_mut();let bitmap=Bitmap(unsafe{CreateDIBSection(None,&info,DIB_RGB_COLORS,&mut bits,None,0)}?);
    let rgb=[0u8,0,0,80,40,20,0,0];unsafe{std::ptr::copy_nonoverlapping(rgb.as_ptr(),bits.cast::<u8>(),rgb.len());}
    assert_eq!(wic_pixels(&gfx,bitmap.0,classify_alpha(bitmap.0,WTSAT_RGB)),[0,0,0,255,80,40,20,255],"native 24-bit DIB colors changed");
    Ok(())
}
