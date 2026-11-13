//! Direct2D / DirectWrite drawing shared by the Explorer chrome and desktop fences.
//!
//! Everything is drawn in physical pixels (render-target DPI fixed to 96) so
//! hairlines and child-window edges stay on the pixel grid at every scale.
//! A single DC render target is bound to an application-owned 32-bit DIB; the
//! caller presents it with `BitBlt` or `UpdateLayeredWindow`, preserving
//! per-pixel alpha for DWM frames and layered desktop windows.

use std::{cell::RefCell, collections::HashMap};

use windows::{
    core::{Interface, PCWSTR, Result, w},
    Win32::{
        Foundation::{RECT, HWND},
        Graphics::{
            Direct2D::{
                Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
                    D2D1_FIGURE_END_CLOSED, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D1_BEZIER_SEGMENT},
                D2D1CreateFactory, ID2D1Bitmap, ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush,
                D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
                D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
            },
            DirectWrite::{
                DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
                DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
                DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
                DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT_CENTER,
                DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS,
                DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_TRIMMING_GRANULARITY_NONE,
                DWRITE_WORD_WRAPPING_NO_WRAP, DWRITE_WORD_WRAPPING_WRAP, DWRITE_WORD_WRAPPING_EMERGENCY_BREAK,
            },
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Gdi::{
                CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
                BITMAPINFOHEADER, BITMAP, DIBSECTION, GetObjectW, GetBitmapBits, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
            },
            Imaging::{
                CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmapSource,
                IWICImagingFactory, WICBitmapAlphaChannelOption, WICBitmapDitherTypeNone,
                WICBitmapIgnoreAlpha, WICBitmapPaletteTypeMedianCut, WICBitmapUseAlpha,
                WICBitmapUsePremultipliedAlpha,
            },
        },
        System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
        UI::WindowsAndMessaging::HICON,
    },
};
use windows_numerics::Vector2;

/// Straight (non-premultiplied) RGBA color.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color { pub r: f32, pub g: f32, pub b: f32, pub a: f32 }

impl Color {
    pub const TRANSPARENT: Self = Self { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
    pub const fn rgba8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: a as f32 / 255.0 }
    }
    pub fn with_alpha(self, a: f32) -> Self { Self { a: (self.a * a).clamp(0.0, 1.0), ..self } }
    fn d2d(self) -> D2D1_COLOR_F { D2D1_COLOR_F { r: self.r, g: self.g, b: self.b, a: self.a } }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RectF { pub x: f32, pub y: f32, pub w: f32, pub h: f32 }

impl RectF {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self { Self { x, y, w, h } }
    pub fn right(&self) -> f32 { self.x + self.w }
    pub fn bottom(&self) -> f32 { self.y + self.h }
    pub fn inset(&self, dx: f32, dy: f32) -> Self {
        Self { x: self.x + dx, y: self.y + dy, w: (self.w - dx * 2.0).max(0.0), h: (self.h - dy * 2.0).max(0.0) }
    }
    pub fn contains(&self, x: f32, y: f32) -> bool { x >= self.x && y >= self.y && x < self.right() && y < self.bottom() }
    fn d2d(&self) -> D2D_RECT_F { D2D_RECT_F { left: self.x, top: self.y, right: self.x + self.w, bottom: self.y + self.h } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align { Start, Center, End }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Font { Ui, Icons }

/// Text appearance. `size` is in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    pub font: Font,
    pub size: f32,
    pub weight: u16,
    pub align: Align,
    pub center_vertically: bool,
    pub wrap: bool,
    pub ellipsis: bool,
}

impl TextStyle {
    pub const fn ui(size: f32) -> Self {
        Self { font: Font::Ui, size, weight: 400, align: Align::Start, center_vertically: true, wrap: false, ellipsis: true }
    }
    pub const fn icon(size: f32) -> Self {
        Self { font: Font::Icons, size, weight: 400, align: Align::Center, center_vertically: true, wrap: false, ellipsis: false }
    }
    pub const fn weight(mut self, weight: u16) -> Self { self.weight = weight; self }
    pub const fn align(mut self, align: Align) -> Self { self.align = align; self }
    pub const fn top(mut self) -> Self { self.center_vertically = false; self }
    pub const fn wrapped(mut self) -> Self { self.wrap = true; self }
    fn key(&self) -> FormatKey {
        FormatKey { font: self.font, size: (self.size * 100.0) as u32, weight: self.weight, align: self.align,
            center: self.center_vertically, wrap: self.wrap, ellipsis: self.ellipsis }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FormatKey { font: Font, size: u32, weight: u16, align: Align, center: bool, wrap: bool, ellipsis: bool }

/// Owner of the device-independent factories and one DC render target.
pub struct Gfx {
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    wic: RefCell<Option<IWICImagingFactory>>,
    target: RefCell<Option<(ID2D1DCRenderTarget, ID2D1SolidColorBrush)>>,
    formats: RefCell<HashMap<FormatKey, (u64, IDWriteTextFormat)>>,
    format_clock: std::cell::Cell<u64>,
    ui_family: Vec<u16>,
    icon_family: Vec<u16>,
    generation: std::cell::Cell<u64>,
}

impl Gfx {
    /// Creates the factories. Must be called on a COM-initialized UI thread.
    pub fn new(ui_family: &str) -> Result<Self> {
        let d2d: ID2D1Factory = unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let mut gfx = Self { d2d, dwrite, wic: RefCell::new(None), target: RefCell::new(None), formats: RefCell::new(HashMap::new()),
            ui_family: wide(ui_family), icon_family: wide("Segoe MDL2 Assets"), generation: std::cell::Cell::new(1), format_clock: std::cell::Cell::new(0) };
        // Windows 11 ships Segoe Fluent Icons; Windows 10 / Server keep the MDL2 glyph font with the same code points.
        if gfx.family_exists("Segoe Fluent Icons") { gfx.icon_family = wide("Segoe Fluent Icons"); }
        if !gfx.family_exists(ui_family) { gfx.ui_family = wide("Segoe UI"); }
        Ok(gfx)
    }

    /// Bitmaps created through this object belong to the current render target.
    /// The generation changes whenever the target is recreated.
    pub fn generation(&self) -> u64 { self.generation.get() }

    /// Releases the DC target's backing allocation while no content is visible.
    /// Callers discard target-owned bitmaps first; WIC data can be uploaded again.
    pub fn release_device_resources(&self) {
        if self.target.borrow_mut().take().is_some() {
            self.generation.set(self.generation.get().wrapping_add(1));
        }
        self.wic.borrow_mut().take();
    }

    pub fn cache_stats(&self) -> (usize, bool) { (self.formats.borrow().len(), self.target.borrow().is_some()) }

    pub fn imaging_factory_live(&self) -> bool { self.wic.borrow().is_some() }

    fn imaging_factory(&self) -> Option<IWICImagingFactory> {
        if let Some(factory) = self.wic.borrow().as_ref() { return Some(factory.clone()); }
        let factory: IWICImagingFactory = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()? };
        *self.wic.borrow_mut() = Some(factory.clone());
        Some(factory)
    }

    pub fn family_exists(&self, family: &str) -> bool {
        let mut collection = None;
        if unsafe { self.dwrite.GetSystemFontCollection(&mut collection, false) }.is_err() { return false; }
        let Some(collection) = collection else { return false };
        let name = wide(family);
        let (mut index, mut exists) = (0u32, windows::core::BOOL(0));
        unsafe { collection.FindFamilyName(PCWSTR(name.as_ptr()), &mut index, &mut exists) }.is_ok() && exists.as_bool()
    }

    fn target(&self) -> Result<(ID2D1DCRenderTarget, ID2D1SolidColorBrush)> {
        if let Some(pair) = self.target.borrow().as_ref() { return Ok(pair.clone()); }
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            // Software rasterizer: the result is read back into a DIB anyway, and a GPU
            // target would load the full Direct3D/driver stack (tens of MB) per process.
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0, dpiY: 96.0, usage: D2D1_RENDER_TARGET_USAGE_NONE, minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let target = unsafe { self.d2d.CreateDCRenderTarget(&props)? };
        let brush = unsafe { target.CreateSolidColorBrush(&Color::TRANSPARENT.d2d(), None)? };
        unsafe { target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE); }
        *self.target.borrow_mut() = Some((target.clone(), brush.clone()));
        Ok((target, brush))
    }

    /// Draws into `surface`, clearing it to fully transparent first.
    pub fn draw(&self, surface: &Surface, paint: impl FnOnce(&Canvas)) -> Result<()> {
        let (target, brush) = self.target()?;
        let rect = RECT { left: 0, top: 0, right: surface.width, bottom: surface.height };
        unsafe {
            target.BindDC(surface.dc, &rect)?;
            target.BeginDraw();
            target.Clear(Some(&Color::TRANSPARENT.d2d()));
        }
        let canvas = Canvas { gfx: self, target: &target, brush: &brush };
        paint(&canvas);
        let result = unsafe { target.EndDraw(None, None) };
        if let Err(error) = &result {
            // D2DERR_RECREATE_TARGET: drop the target and every bitmap made from it.
            if error.code().0 as u32 == 0x8899_000C {
                *self.target.borrow_mut() = None;
                self.generation.set(self.generation.get() + 1);
            }
        }
        result
    }

    fn format(&self, style: &TextStyle) -> Option<IDWriteTextFormat> {
        let key = style.key();
        let tick = self.format_clock.get().wrapping_add(1);
        self.format_clock.set(tick);
        if let Some((used, format)) = self.formats.borrow_mut().get_mut(&key) { *used = tick; return Some(format.clone()); }
        let family = match style.font { Font::Ui => &self.ui_family, Font::Icons => &self.icon_family };
        let format = unsafe {
            self.dwrite.CreateTextFormat(PCWSTR(family.as_ptr()), None, DWRITE_FONT_WEIGHT(style.weight as i32),
                DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, style.size.max(1.0), w!("zh-CN")).ok()?
        };
        unsafe {
            let _ = format.SetTextAlignment(match style.align {
                Align::Start => DWRITE_TEXT_ALIGNMENT_LEADING, Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::End => DWRITE_TEXT_ALIGNMENT_TRAILING });
            let _ = format.SetParagraphAlignment(if style.center_vertically { DWRITE_PARAGRAPH_ALIGNMENT_CENTER } else { DWRITE_PARAGRAPH_ALIGNMENT_NEAR });
            if !style.wrap { let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP); }
            else if format.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK).is_err() {
                // Older DirectWrite only knows plain wrapping; long unbroken names are trimmed instead.
                let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
            }
            if style.ellipsis {
                let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
                if let Ok(sign) = self.dwrite.CreateEllipsisTrimmingSign(&format) { let _ = format.SetTrimming(&trimming, &sign); }
            } else {
                let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_NONE, delimiter: 0, delimiterCount: 0 };
                let _ = format.SetTrimming(&trimming, None);
            }
        }
        let mut formats = self.formats.borrow_mut();
        if formats.len() >= 64 {
            if let Some(oldest) = formats.iter().min_by_key(|(_, (used, _))| *used).map(|(key, _)| *key) { formats.remove(&oldest); }
        }
        formats.insert(key, (tick, format.clone()));
        Some(format)
    }

    /// Measures text; returns (width, height) in pixels.
    pub fn measure(&self, text: &str, style: &TextStyle, max_width: f32) -> (f32, f32) {
        let Some(format) = self.format(style) else { return (0.0, 0.0) };
        let text = utf16(text);
        let Ok(layout) = (unsafe { self.dwrite.CreateTextLayout(&text, &format, max_width.max(1.0), 4096.0) }) else { return (0.0, 0.0) };
        let mut metrics = DWRITE_TEXT_METRICS::default();
        if unsafe { layout.GetMetrics(&mut metrics) }.is_err() { return (0.0, 0.0); }
        (metrics.widthIncludingTrailingWhitespace.ceil(), metrics.height.ceil())
    }

    /// Converts an icon into a WIC bitmap that can later be uploaded to the target.
    pub fn wic_from_icon(&self, icon: HICON) -> Option<IWICBitmapSource> {
        let bitmap = unsafe { self.imaging_factory()?.CreateBitmapFromHICON(icon).ok()? };
        self.convert(&bitmap.cast().ok()?)
    }

    /// Converts a 32-bit HBITMAP. `alpha` selects straight alpha, premultiplied alpha or opaque pixels.
    pub fn wic_from_hbitmap(&self, bitmap: HBITMAP, alpha: BitmapAlpha) -> Option<IWICBitmapSource> {
        let mut section=DIBSECTION::default();
        let read=unsafe{GetObjectW(bitmap.into(),std::mem::size_of::<DIBSECTION>()as i32,Some((&mut section as*mut DIBSECTION).cast()))};
        if read as usize==std::mem::size_of::<BITMAP>()&&section.dsBm.bmBitsPixel==24&&section.dsBm.bmPlanes==1{
            return self.wic_from_rgb_ddb(bitmap,&section.dsBm);
        }
        let option: WICBitmapAlphaChannelOption = match alpha {
            BitmapAlpha::Straight => WICBitmapUseAlpha, BitmapAlpha::Premultiplied => WICBitmapUsePremultipliedAlpha,
            BitmapAlpha::Ignore => WICBitmapIgnoreAlpha };
        let source = unsafe { self.imaging_factory()?.CreateBitmapFromHBITMAP(bitmap, Default::default(), option).ok()? };
        self.convert(&source.cast().ok()?)
    }

    fn wic_from_rgb_ddb(&self,bitmap:HBITMAP,metadata:&BITMAP)->Option<IWICBitmapSource>{
        // GDI may retain a 24-bit DDB's bytes although the current display
        // cannot convert that device format through GetDIBits. WIC can report
        // success with black pixels in that case. Normalize this known RGB
        // layout explicitly; the unused alpha byte is always fully opaque.
        let width=usize::try_from(metadata.bmWidth).ok().filter(|width|*width>0)?;
        let height=usize::try_from(metadata.bmHeight).ok().filter(|height|*height>0)?;
        let stride=usize::try_from(metadata.bmWidthBytes).ok()?;
        let rgb_row=width.checked_mul(3)?;if stride<rgb_row{return None;}
        let size=stride.checked_mul(height)?;let output_size=width.checked_mul(height)?.checked_mul(4)?;
        if size>8*1024*1024||output_size>8*1024*1024{return None;}
        let mut rgb=vec![0u8;size];
        if unsafe{GetBitmapBits(bitmap,size as i32,rgb.as_mut_ptr().cast())}!=size as i32{return None;}
        let mut bgra=vec![0u8;output_size];
        for (source,destination) in rgb.chunks_exact(stride).zip(bgra.chunks_exact_mut(width*4)){
            for (source,destination) in source[..rgb_row].chunks_exact(3).zip(destination.chunks_exact_mut(4)){
                destination[..3].copy_from_slice(source);destination[3]=255;
            }
        }
        let source=unsafe{self.imaging_factory()?.CreateBitmapFromMemory(width as u32,height as u32,&GUID_WICPixelFormat32bppPBGRA,(width*4)as u32,&bgra)}.ok()?;
        self.convert(&source.cast().ok()?)
    }

    fn convert(&self, source: &IWICBitmapSource) -> Option<IWICBitmapSource> {
        let factory = self.imaging_factory()?;
        let converter = unsafe { factory.CreateFormatConverter().ok()? };
        unsafe { converter.Initialize(source, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeMedianCut).ok()? };
        // Materialize so the source HBITMAP/HICON can be destroyed immediately.
        let bitmap = unsafe { factory.CreateBitmapFromSource(&converter, windows::Win32::Graphics::Imaging::WICBitmapCacheOnLoad).ok()? };
        bitmap.cast().ok()
    }

    /// Uploads a WIC bitmap to the current render target.
    pub fn upload(&self, source: &IWICBitmapSource) -> Option<ID2D1Bitmap> {
        let (target, _) = self.target().ok()?;
        unsafe { target.CreateBitmapFromWicBitmap(source, None).ok() }
    }

    pub fn wic_size(source: &IWICBitmapSource) -> (u32, u32) {
        let (mut w, mut h) = (0, 0);
        let _ = unsafe { source.GetSize(&mut w, &mut h) };
        (w, h)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitmapAlpha { Straight, Premultiplied, Ignore }

/// Drawing operations in pixel coordinates.
pub struct Canvas<'a> {
    gfx: &'a Gfx,
    target: &'a ID2D1DCRenderTarget,
    brush: &'a ID2D1SolidColorBrush,
}

impl Canvas<'_> {
    pub fn gfx(&self) -> &Gfx { self.gfx }
    fn brush(&self, color: Color) -> &ID2D1SolidColorBrush { unsafe { self.brush.SetColor(&color.d2d()); } self.brush }
    pub fn clear_rect(&self, rect: RectF) {
        // Replace pixels (not blend) so DWM frame regions become truly transparent.
        unsafe {
            self.target.PushAxisAlignedClip(&rect.d2d(), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            self.target.Clear(Some(&Color::TRANSPARENT.d2d()));
            self.target.PopAxisAlignedClip();
        }
    }
    pub fn fill(&self, rect: RectF, color: Color) {
        if color.a <= 0.0 || rect.w <= 0.0 || rect.h <= 0.0 { return; }
        unsafe { self.target.FillRectangle(&rect.d2d(), self.brush(color)); }
    }
    pub fn round(&self, rect: RectF, radius: f32, color: Color) {
        if color.a <= 0.0 || rect.w <= 0.0 || rect.h <= 0.0 { return; }
        let rounded = D2D1_ROUNDED_RECT { rect: rect.d2d(), radiusX: radius, radiusY: radius };
        unsafe { self.target.FillRoundedRectangle(&rounded, self.brush(color)); }
    }
    pub fn stroke_round(&self, rect: RectF, radius: f32, color: Color, width: f32) {
        if color.a <= 0.0 { return; }
        let half = width / 2.0;
        let rounded = D2D1_ROUNDED_RECT { rect: rect.inset(half, half).d2d(), radiusX: radius, radiusY: radius };
        unsafe { self.target.DrawRoundedRectangle(&rounded, self.brush(color), width, None); }
    }
    pub fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32, color: Color, width: f32) {
        if color.a <= 0.0 { return; }
        unsafe { self.target.DrawLine(Vector2 { X: x1, Y: y1 }, Vector2 { X: x2, Y: y2 }, self.brush(color), width, None); }
    }
    pub fn text(&self, text: &str, rect: RectF, style: &TextStyle, color: Color) {
        if text.is_empty() || color.a <= 0.0 || rect.w <= 0.0 { return; }
        let Some(format) = self.gfx.format(style) else { return };
        let text = utf16(text);
        unsafe {
            self.target.DrawText(&text, &format, &rect.d2d(), self.brush(color),
                D2D1_DRAW_TEXT_OPTIONS_CLIP | D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, DWRITE_MEASURING_MODE_NATURAL);
        }
    }
    /// Draws one icon-font glyph centered in `rect`.
    pub fn glyph(&self, glyph: &str, rect: RectF, size: f32, color: Color) {
        self.text(glyph, rect, &TextStyle::icon(size), color);
    }
    pub fn image(&self, bitmap: &ID2D1Bitmap, rect: RectF, opacity: f32) {
        unsafe { self.target.DrawBitmap(bitmap, Some(&rect.d2d()), opacity, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None); }
    }
    pub fn push_clip(&self, rect: RectF) { unsafe { self.target.PushAxisAlignedClip(&rect.d2d(), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE); } }
    pub fn pop_clip(&self) { unsafe { self.target.PopAxisAlignedClip(); } }

    /// Windows 11 tab silhouette: rounded top corners and concave feet that
    /// merge into the toolbar below.
    pub fn tab_shape(&self, rect: RectF, radius: f32, foot: f32, color: Color) {
        let Ok(path) = (unsafe { self.gfx.d2d.CreatePathGeometry() }) else { return };
        let Ok(sink) = (unsafe { path.Open() }) else { return };
        let (l, t, r, b) = (rect.x, rect.y, rect.right(), rect.bottom());
        let k = 0.552_284_8_f32;
        unsafe {
            sink.BeginFigure(Vector2 { X: l - foot, Y: b }, D2D1_FIGURE_BEGIN_FILLED);
            // Left foot (concave quarter circle).
            sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: Vector2 { X: l - foot + foot * k, Y: b }, point2: Vector2 { X: l, Y: b - foot + foot * k }, point3: Vector2 { X: l, Y: b - foot } });
            sink.AddLine(Vector2 { X: l, Y: t + radius });
            sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: Vector2 { X: l, Y: t + radius - radius * k }, point2: Vector2 { X: l + radius - radius * k, Y: t }, point3: Vector2 { X: l + radius, Y: t } });
            sink.AddLine(Vector2 { X: r - radius, Y: t });
            sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: Vector2 { X: r - radius + radius * k, Y: t }, point2: Vector2 { X: r, Y: t + radius - radius * k }, point3: Vector2 { X: r, Y: t + radius } });
            sink.AddLine(Vector2 { X: r, Y: b - foot });
            sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: Vector2 { X: r, Y: b - foot + foot * k }, point2: Vector2 { X: r + foot - foot * k, Y: b }, point3: Vector2 { X: r + foot, Y: b } });
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            let _ = sink.Close();
            self.target.FillGeometry(&path, self.brush(color), None);
        }
    }
}

/// A 32-bit top-down premultiplied DIB selected into a memory DC.
pub struct Surface {
    pub dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pub width: i32,
    pub height: i32,
    pub bits: *mut u8,
}

impl Surface {
    pub fn pixel_bytes(&self) -> u64 { self.width.max(0) as u64 * self.height.max(0) as u64 * 4 }
    pub fn new(width: i32, height: i32) -> Option<Self> {
        let (width, height) = (width.max(1), height.max(1));
        let info = BITMAPINFO { bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: width, biHeight: -height,
            biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() }, ..Default::default() };
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() { return None; }
        let mut bits = std::ptr::null_mut();
        let Ok(bitmap) = (unsafe { CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) }) else {
            unsafe { let _ = DeleteDC(dc); } return None;
        };
        let previous = unsafe { SelectObject(dc, bitmap.into()) };
        Some(Self { dc, bitmap, previous, width, height, bits: bits.cast() })
    }
    /// Reuses the surface when the size is unchanged.
    pub fn ensure(slot: &mut Option<Surface>, width: i32, height: i32) -> Option<&Surface> {
        let (width, height) = (width.max(1), height.max(1));
        if slot.as_ref().is_none_or(|s| s.width != width || s.height != height) { *slot = Surface::new(width, height); }
        slot.as_ref()
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// Reads the system message font family (Microsoft YaHei UI on Chinese systems).
pub fn system_ui_font(hwnd: Option<HWND>) -> (String, f32) {
    use windows::Win32::UI::{HiDpi::SystemParametersInfoForDpi, WindowsAndMessaging::{NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS}};
    let mut metrics = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
    let _ = hwnd;
    let ok = unsafe { SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, metrics.cbSize, Some((&mut metrics as *mut NONCLIENTMETRICSW).cast()), 0, 96) }.is_ok();
    if !ok { return ("Segoe UI".into(), 12.0); }
    let face = &metrics.lfMessageFont.lfFaceName;
    let end = face.iter().position(|&c| c == 0).unwrap_or(face.len());
    let name = String::from_utf16_lossy(&face[..end]);
    let height = metrics.lfMessageFont.lfHeight.unsigned_abs() as f32;
    (if name.is_empty() { "Segoe UI".into() } else { name }, if height > 0.0 { height } else { 12.0 })
}

fn wide(text: &str) -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() }
fn utf16(text: &str) -> Vec<u16> { text.encode_utf16().collect() }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rect_helpers_never_go_negative() {
        let r = RectF::new(0.0, 0.0, 4.0, 4.0).inset(3.0, 3.0);
        assert_eq!((r.w, r.h), (0.0, 0.0));
        assert!(RectF::new(10.0, 10.0, 5.0, 5.0).contains(12.0, 14.9));
        assert!(!RectF::new(10.0, 10.0, 5.0, 5.0).contains(15.0, 12.0));
    }
    #[test]
    fn color_alpha_scales() {
        let c = Color::rgba8(255, 0, 0, 128).with_alpha(0.5);
        assert!((c.a - 0.251).abs() < 0.01);
    }
}
