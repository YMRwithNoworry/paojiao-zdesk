//! Display-sized WIC decoding. Large JPEGs must use codec-native downsampling.
use std::{path::Path, ptr};
use anyhow::{bail, ensure, Context, Result};
use windows::{core::{GUID, Interface, PCWSTR}, Win32::{Graphics::Imaging::*,
    System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER, StructuredStorage::{PROPVARIANT, PropVariantClear, PropVariantToUInt16}}}};
use super::{fit_dimensions, read_stream, wide, MAX_BITMAP_WIDTH, MAX_BITMAP_HEIGHT};

// The source header limit prevents absurd codec work. It is not an allocation
// budget: every native intermediate is checked separately before CopyPixels.
const MAX_SOURCE_PIXELS:u64=512_000_000;
const MAX_GENERIC_PIXELS:u64=20_000_000;
const MAX_NATIVE_BYTES:u64=16*1024*1024;

#[derive(Debug)]
pub(super) struct ImagePreview {pub width:u32,pub height:u32,pub pixels:Vec<u8>,
    #[cfg(test)]pub stats:DecodeStats}
#[cfg(test)]
#[derive(Debug,serde::Serialize)]
pub(super) struct DecodeStats {source:[u32;2],native:[u32;2],orientation:u16,color_managed:bool}

pub(super) fn dimensions_allowed(width:u32,height:u32)->bool{
    width>0&&height>0&&width<=100_000&&height<=100_000&&u64::from(width)*u64::from(height)<=MAX_SOURCE_PIXELS
}
fn checked_bytes(width:u32,height:u32,bytes_per_pixel:u32,limit:u64)->Result<(u32,usize)>{
    ensure!(width>0&&height>0,"图片尺寸无效");
    let stride=width.checked_mul(bytes_per_pixel).context("图片宽度溢出")?;
    let bytes=u64::from(stride)*u64::from(height);
    ensure!(bytes<=limit&&bytes<=u64::from(u32::MAX),"图片解码缓冲超出预览范围");
    Ok((stride,bytes as usize))
}
fn cancelled(check:&impl Fn()->bool)->Result<()>{if check(){bail!("预览已取消");}Ok(())}
fn orientation(frame:&IWICBitmapFrameDecode)->u16{
    let Ok(reader)=(unsafe{frame.GetMetadataQueryReader()})else{return 1;};
    for query in ["/app1/ifd/{ushort=274}","/ifd/{ushort=274}","/eXIf/ifd/{ushort=274}"]{
        let mut value=PROPVARIANT::default();
        let result=unsafe{reader.GetMetadataByName(PCWSTR(wide(query).as_ptr()),&mut value)}
            .and_then(|_|unsafe{PropVariantToUInt16(&value)});
        let _=unsafe{PropVariantClear(&mut value)};
        if let Ok(value)=result{if (1..=8).contains(&value){return value;}}
    }1
}
fn native_pixel_bytes(format:GUID)->Option<u32>{
    if [GUID_WICPixelFormat24bppBGR,GUID_WICPixelFormat24bppRGB].contains(&format){Some(3)}
    else if [GUID_WICPixelFormat32bppBGRA,GUID_WICPixelFormat32bppBGR,GUID_WICPixelFormat32bppPBGRA,
        GUID_WICPixelFormat32bppRGBA,GUID_WICPixelFormat32bppPRGBA,GUID_WICPixelFormat32bppCMYK].contains(&format){Some(4)}
    else if format==GUID_WICPixelFormat8bppGray{Some(1)}else{None}
}

fn native_source(factory:&IWICImagingFactory,frame:&IWICBitmapFrameDecode,target:(u32,u32),original:(u32,u32),check:&impl Fn()->bool)
    ->Result<Option<(IWICBitmapSource,(u32,u32))>>{
    let Ok(transform)=frame.cast::<IWICBitmapSourceTransform>()else{return Ok(None);};
    let(mut width,mut height)=target;
    unsafe{transform.GetClosestSize(&mut width,&mut height)}?;
    if (width,height)==original{return Ok(None);}
    ensure!(width<=original.0&&height<=original.1&&width>=target.0&&height>=target.1,"图片解码器返回了无效缩放尺寸");
    // Preserve the decoder's color model until its ICC profile is applied.
    let mut format=unsafe{frame.GetPixelFormat()}?;
    unsafe{transform.GetClosestPixelFormat(&mut format)}?;
    let Some(bpp)=native_pixel_bytes(format)else{return Ok(None);};
    if !unsafe{transform.DoesSupportTransform(WICBitmapTransformRotate0)}?.as_bool(){return Ok(None);}
    let(stride,bytes)=checked_bytes(width,height,bpp,MAX_NATIVE_BYTES)?;
    cancelled(check)?;
    let mut pixels=vec![0u8;bytes];
    unsafe{transform.CopyPixels(ptr::null(),width,height,&format,WICBitmapTransformRotate0,stride,&mut pixels)}?;
    cancelled(check)?;
    // WIC owns its copy; the temporary native decode buffer is released before
    // the final display bitmap and color/rotation transforms are materialized.
    let bitmap=unsafe{factory.CreateBitmapFromMemory(width,height,&format,stride,&pixels)}?;
    drop(pixels);
    Ok(Some((bitmap.cast()?,(width,height))))
}

fn apply_color(factory:&IWICImagingFactory,frame:&IWICBitmapFrameDecode,source:&IWICBitmapSource)->Result<(IWICBitmapSource,bool)>{
    let mut count=0;
    if unsafe{frame.GetColorContexts(&mut [],&mut count)}.is_ok()&&count>0{
        ensure!(count<=16,"图片颜色配置过多");
        let mut contexts=Vec::with_capacity(count as usize);
        for _ in 0..count{contexts.push(Some(unsafe{factory.CreateColorContext()}?));}
        unsafe{frame.GetColorContexts(&mut contexts,&mut count)}?;
        let destination=unsafe{factory.CreateColorContext()}?;
        unsafe{destination.InitializeFromExifColorSpace(1)}?;
        // Prefer an embedded ICC profile over a generic EXIF color-space tag.
        contexts.sort_by_key(|context|context.as_ref().is_none_or(|context|unsafe{context.GetType()}.ok()!=Some(WICColorContextProfile)));
        for context in contexts.into_iter().flatten(){
            let kind=unsafe{context.GetType()}?;
            if kind==WICColorContextExifColorSpace&&unsafe{context.GetExifColorSpace()}?==65535{continue;}
            if kind==WICColorContextUninitialized{continue;}
            let conversion=unsafe{factory.CreateColorTransformer()}?;
            unsafe{conversion.Initialize(source,&context,&destination,&GUID_WICPixelFormat32bppBGRA)}
                .context("无法应用图片颜色配置")?;
            return Ok((conversion.cast()?,true));
        }
    }
    let converter=unsafe{factory.CreateFormatConverter()}?;
    unsafe{converter.Initialize(source,&GUID_WICPixelFormat32bppBGRA,WICBitmapDitherTypeNone,None,0.0,WICBitmapPaletteTypeCustom)}?;
    Ok((converter.cast()?,false))
}

fn orient(pixels:Vec<u8>,width:u32,height:u32,orientation:u16)->(Vec<u8>,u32,u32){
    if orientation==1{return(pixels,width,height);}
    let(out_width,out_height)=if orientation>=5{(height,width)}else{(width,height)};
    let mut output=vec![0u8;pixels.len()];
    for y in 0..height{for x in 0..width{
        let(dx,dy)=match orientation{2=>(width-1-x,y),3=>(width-1-x,height-1-y),4=>(x,height-1-y),
            5=>(y,x),6=>(height-1-y,x),7=>(height-1-y,width-1-x),8=>(y,width-1-x),_=>(x,y)};
        let from=((u64::from(y)*u64::from(width)+u64::from(x))*4)as usize;
        let to=((u64::from(dy)*u64::from(out_width)+u64::from(dx))*4)as usize;
        output[to..to+4].copy_from_slice(&pixels[from..from+4]);
    }}(output,out_width,out_height)
}

pub(super) fn decode(path:&Path,background:u32,bounds:(u32,u32),check:impl Fn()->bool)->Result<ImagePreview>{
    cancelled(&check)?;
    let factory:IWICImagingFactory=unsafe{CoCreateInstance(&CLSID_WICImagingFactory,None,CLSCTX_INPROC_SERVER)}?;
    let stream=read_stream(path)?;
    let decoder=unsafe{factory.CreateDecoderFromStream(&stream,ptr::null(),WICDecodeMetadataCacheOnDemand)}?;
    let frame=unsafe{decoder.GetFrame(0)}?;
    let(mut width,mut height)=(0,0);unsafe{frame.GetSize(&mut width,&mut height)}?;
    ensure!(dimensions_allowed(width,height),"图片像素尺寸超出快速预览范围");
    let orientation=orientation(&frame);
    let bounds=(bounds.0.clamp(1,MAX_BITMAP_WIDTH),bounds.1.clamp(1,MAX_BITMAP_HEIGHT));
    let raw_bounds=if orientation>=5{(bounds.1,bounds.0)}else{bounds};
    let target=fit_dimensions(width,height,raw_bounds.0,raw_bounds.1);
    cancelled(&check)?;
    let(source,native)=match native_source(&factory,&frame,target,(width,height),&check)?{
        Some(source)=>source,
        None=>{
            ensure!(u64::from(width)*u64::from(height)<=MAX_GENERIC_PIXELS,"此图片解码器不支持受控缩放，请使用支持此格式的图片查看器");
            (frame.cast::<IWICBitmapSource>()?,(width,height))
        }
    };
    let scaled:IWICBitmapSource=if native!=target{
        let scaler=unsafe{factory.CreateBitmapScaler()}?;
        unsafe{scaler.Initialize(&source,target.0,target.1,WICBitmapInterpolationModeFant)}?;scaler.cast()?
    }else{source};
    let(converted,_color_managed)=apply_color(&factory,&frame,&scaled)?;
    let(stride,bytes)=checked_bytes(target.0,target.1,4,u64::from(MAX_BITMAP_WIDTH)*u64::from(MAX_BITMAP_HEIGHT)*4)?;
    let mut pixels=vec![0u8;bytes];
    // Copy small output stripes, allowing a superseded request to stop between
    // stripes without exposing codecs or COM pointers to the manager process.
    for y in (0..target.1).step_by(64){
        cancelled(&check)?;
        let rows=(target.1-y).min(64);
        let rect=WICRect{X:0,Y:y as i32,Width:target.0 as i32,Height:rows as i32};
        let start=(y*stride)as usize;let end=((y+rows)*stride)as usize;
        unsafe{converted.CopyPixels(&rect,stride,&mut pixels[start..end])}?;
    }
    cancelled(&check)?;
    let bg=[(background>>16)as u8,(background>>8)as u8,background as u8];
    for pixel in pixels.chunks_exact_mut(4){
        let alpha=u32::from(pixel[3]);
        for channel in 0..3{pixel[channel]=((u32::from(pixel[channel])*alpha+u32::from(bg[channel])*(255-alpha)+127)/255)as u8;}
        pixel[3]=255;
    }
    let(pixels,out_width,out_height)=orient(pixels,target.0,target.1,orientation);
    Ok(ImagePreview{width:out_width,height:out_height,pixels,
        #[cfg(test)]stats:DecodeStats{source:[width,height],native:[native.0,native.1],orientation,color_managed:_color_managed}})
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn pixel_budgets_are_checked_before_allocation(){
        assert!(dimensions_allowed(9203,6135));assert!(!dimensions_allowed(u32::MAX,1));
        assert!(checked_bytes(9203,6135,4,MAX_NATIVE_BYTES).is_err());
        assert!(checked_bytes(1151,767,3,MAX_NATIVE_BYTES).is_ok());
        assert!(checked_bytes(u32::MAX,u32::MAX,4,MAX_NATIVE_BYTES).is_err());
        assert!(checked_bytes(0,100,4,MAX_NATIVE_BYTES).is_err());
        assert!(decode(Path::new("missing.jpg"),0,(800,600),||true).unwrap_err().to_string().contains("取消"));
    }
    #[test]fn exif_orientations_preserve_all_pixels(){
        let source=[1u8,2,3,4,5,6].into_iter().flat_map(|value|[value,0,0,255]).collect::<Vec<_>>();
        for(orientation,expected)in [(1,vec![1,2,3,4,5,6]),(2,vec![3,2,1,6,5,4]),(3,vec![6,5,4,3,2,1]),(4,vec![4,5,6,1,2,3]),
            (5,vec![1,4,2,5,3,6]),(6,vec![4,1,5,2,6,3]),(7,vec![6,3,5,2,4,1]),(8,vec![3,6,2,5,1,4])]{
            let(pixels,width,height)=orient(source.clone(),3,2,orientation);
            assert_eq!((width,height),if orientation>=5{(2,3)}else{(3,2)});
            assert_eq!(pixels.chunks_exact(4).map(|pixel|pixel[0]).collect::<Vec<_>>(),expected);
        }
    }

    #[repr(C)]#[derive(Default)]
    struct MemoryCounters{size:u32,faults:u32,values:[usize;9]}
    #[link(name="kernel32")]
    unsafe extern "system"{fn K32GetProcessMemoryInfo(process:windows::Win32::Foundation::HANDLE,counters:*mut MemoryCounters,size:u32)->i32;}
    fn private_bytes()->Result<usize>{
        let mut counters=MemoryCounters{size:std::mem::size_of::<MemoryCounters>()as u32,..Default::default()};
        let size=counters.size;
        ensure!(unsafe{K32GetProcessMemoryInfo(windows::Win32::System::Threading::GetCurrentProcess(),&mut counters,size)}!=0,"process memory read failed");
        Ok(counters.values[8])
    }
    fn sample(image:&ImagePreview,x:u32,y:u32)->[u8;3]{
        let offset=((y*image.width+x)*4)as usize;image.pixels[offset..offset+3].try_into().unwrap()
    }
    fn wic_reference_pixel(path:&Path)->Result<[u8;3]>{
        // An untagged device-CMYK value has no universal RGB equivalent. Use
        // the installed WIC codec's direct, unscaled conversion as the native
        // baseline, independently of our downsampling/ICC/orientation pipeline.
        let factory:IWICImagingFactory=unsafe{CoCreateInstance(&CLSID_WICImagingFactory,None,CLSCTX_INPROC_SERVER)}?;
        let stream=read_stream(path)?;let decoder=unsafe{factory.CreateDecoderFromStream(&stream,ptr::null(),WICDecodeMetadataCacheOnDemand)}?;
        let frame=unsafe{decoder.GetFrame(0)}?;let converter=unsafe{factory.CreateFormatConverter()}?;
        unsafe{converter.Initialize(&frame,&GUID_WICPixelFormat32bppBGRA,WICBitmapDitherTypeNone,None,0.0,WICBitmapPaletteTypeCustom)}?;
        let mut pixel=[0u8;4];let rect=WICRect{X:40,Y:30,Width:1,Height:1};
        unsafe{converter.CopyPixels(&rect,4,&mut pixel)}?;Ok(pixel[..3].try_into().unwrap())
    }
    fn corners(image:&ImagePreview,expected:[[u8;3];4])->Result<()>{
        // Quarter-width lands at source x=6902 beside the fixture's x=6900
        // white grid line. Native 1/8 JPEG downsampling correctly mixes that
        // line into the sample; choose interiors away from every grid instead.
        for((x,y),expected)in [(image.width/5,image.height/5),(image.width*7/10,image.height/5),
            (image.width/5,image.height*7/10),(image.width*7/10,image.height*7/10)].into_iter().zip(expected){
            let actual=sample(image,x,y);
            ensure!(actual.into_iter().zip(expected).all(|(a,b)|a.abs_diff(b)<=16),"orientation/color mismatch at ({x},{y}) in {}x{}: {actual:?}, expected {expected:?}",image.width,image.height);
        }Ok(())
    }
    #[test]
    #[ignore="Requires generated large-image fixtures below ZSDESK_NATIVE_TEST_OUTPUT; bounded native WIC decode"]
    fn native_large_jpeg_preview_is_scaled_oriented_and_bounded()->Result<()>{
        use std::{fs,path::PathBuf,sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}},thread,time::{Duration,Instant}};
        use windows::Win32::System::Ole::{OleInitialize,OleUninitialize};
        let root=PathBuf::from(std::env::var_os("ZSDESK_NATIVE_TEST_OUTPUT").context("isolated output required")?).join("large-image");
        ensure!(root.is_absolute(),"absolute output required");
        let fixture=root.join("large-9203x6135.jpg");let rotated=root.join("large-orientation-6.jpg");
        ensure!(fixture.is_file()&&rotated.is_file(),"generate large JPEG fixtures first");
        let before=[fs::read(&fixture)?,fs::read(&rotated)?];
        unsafe{OleInitialize(None)}?;
        struct Apartment;impl Drop for Apartment{fn drop(&mut self){unsafe{OleUninitialize();}}}let _apartment=Apartment;
        let initial=private_bytes()?;let stop=Arc::new(AtomicBool::new(false));let peak=Arc::new(AtomicUsize::new(initial));
        let worker_stop=stop.clone();let worker_peak=peak.clone();
        let sampler=thread::spawn(move||while !worker_stop.load(Ordering::Acquire){if let Ok(bytes)=private_bytes(){worker_peak.fetch_max(bytes,Ordering::AcqRel);}thread::sleep(Duration::from_millis(2));});
        let started=Instant::now();
        let checks=(||->Result<serde_json::Value>{
            let image=decode(&fixture,0xFFFFFF,(736,514),||false)?;
            ensure!(image.stats.source==[9203,6135]&&image.width<=736&&image.height<=514,"wrong source/display dimensions");
            ensure!(image.stats.native[0]<9203&&image.stats.native[1]<6135,"full-resolution decode was not reduced");
            ensure!(image.stats.color_managed,"embedded ICC profile was ignored");
            ensure!(image.pixels.len()<=736*514*4,"retained buffer exceeds display size");
            corners(&image,[[25,35,230],[50,200,20],[225,45,25],[20,190,230]]).context("landscape color quadrants")?;
            let first=serde_json::json!({"source":image.stats.source,"native":image.stats.native,"display":[image.width,image.height],"retained_bytes":image.pixels.len(),"color_managed":image.stats.color_managed});
            drop(image);
            let image=decode(&rotated,0xFFFFFF,(736,514),||false)?;
            ensure!(image.stats.orientation==6&&image.width<image.height&&image.width<=736&&image.height<=514,"portrait EXIF orientation ignored");
            corners(&image,[[225,45,25],[25,35,230],[20,190,230],[50,200,20]]).context("EXIF 6 color quadrants")?;
            let portrait=serde_json::json!({"source":image.stats.source,"native":image.stats.native,"display":[image.width,image.height],"orientation":image.stats.orientation,"retained_bytes":image.pixels.len()});
            drop(image);
            let alpha=decode(&root.join("transparent.png"),0xFFFFFF,(80,60),||false)?;
            let color=sample(&alpha,40,30);ensure!(color.into_iter().zip([137,147,237]).all(|(a,b)|a.abs_diff(b)<=2),"alpha composition changed: {color:?}");
            let cmyk=decode(&root.join("cmyk-red.jpg"),0xFFFFFF,(80,60),||false)?;
            let reference=wic_reference_pixel(&root.join("cmyk-red.jpg"))?;
            let color=sample(&cmyk,40,30);ensure!(color==reference,"CMYK conversion differs from native unscaled reference: {color:?} vs {reference:?}");
            let scaled_cmyk=decode(&root.join("cmyk-scaled.jpg"),0xFFFFFF,(80,60),||false)?;
            ensure!(scaled_cmyk.stats.native[0]<2400&&scaled_cmyk.stats.native[1]<1800,"CMYK native downsampling was not exercised");
            let scaled_color=sample(&scaled_cmyk,40,30);
            ensure!(scaled_color.into_iter().zip(reference).all(|(a,b)|a.abs_diff(b)<=2),"CMYK downsampling changed the native color interpretation: {scaled_color:?} vs {reference:?}");
            ensure!(decode(&root.join("corrupt.jpg"),0,(736,514),||false).is_err(),"truncated JPEG accepted");
            ensure!(decode(&root.join("oversized-header.jpg"),0,(736,514),||false).is_err(),"oversized JPEG header accepted");
            let calls=std::cell::Cell::new(0);let cancelled=decode(&fixture,0,(736,514),||{calls.set(calls.get()+1);calls.get()>=3});
            ensure!(cancelled.is_err_and(|error|error.to_string().contains("取消")),"superseded decode ignored cancellation");
            Ok(serde_json::json!({"landscape":first,"portrait":portrait,"alpha":true,
                "cmyk":{"native_reference_bgr":reference,"scaled_bgr":scaled_color,"scaled_native_dimensions":scaled_cmyk.stats.native,"matches_native_reference":true},
                "truncated_rejected":true,"oversized_header_rejected":true,"cancellation":true}))
        })();
        stop.store(true,Ordering::Release);sampler.join().map_err(|_|anyhow::anyhow!("memory sampler panicked"))?;
        peak.fetch_max(private_bytes()?,Ordering::AcqRel);
        let growth=peak.load(Ordering::Acquire).saturating_sub(initial);
        let unchanged=fs::read(&fixture)?==before[0]&&fs::read(&rotated)?==before[1];
        let bounded=growth<128*1024*1024;
        let report=serde_json::json!({"state":if checks.is_ok()&&unchanged&&bounded{"passed"}else{"failed"},
            "checks":checks.as_ref().map_err(|error|format!("{error:#}")),"sampled_private_growth":growth,"elapsed_ms":started.elapsed().as_millis(),
            "full_bgra_source_bytes":9203u64*6135*4,"fixture_unchanged":unchanged,"preview_worker_isolation":"production path unchanged"});
        fs::write(root.join("verification.current.json"),serde_json::to_vec_pretty(&report)?)?;println!("{report}");
        checks?;ensure!(unchanged&&bounded,"image data changed or decoder exceeded measured memory bound");Ok(())
    }
}
