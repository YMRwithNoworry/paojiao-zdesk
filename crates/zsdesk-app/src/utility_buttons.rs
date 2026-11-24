//! Native button input and accessibility with retained ZSUI button faces.
use super::*;
use std::collections::VecDeque;
use windows::Win32::UI::Shell::GetWindowSubclass;
use zsdesk_windows::gfx::Surface;
use zsui::{Dpi, Rect, View, ViewLayoutCx, ViewPaintCx, WindowsGdiDrawSink};

const SUBCLASS: usize = 0x5a42;
pub(super) fn owns(id: i32) -> bool {
    matches!(id, 12 | 16 | 17) || (FIRST_FILTER..FIRST_FILTER + FILTERS.len() as i32).contains(&id)
}

pub(super) struct Buttons {
    pub selected: Cell<usize>,
    theme: Cell<Palette>,
    dpi: Cell<u32>,
}
impl Default for Buttons {
    fn default() -> Self {
        Self {
            selected: Cell::new(0),
            theme: Cell::new(Palette::light()),
            dpi: Cell::new(96),
        }
    }
}
impl Buttons {
    pub fn appearance(&self, p: Palette, dpi: u32) {
        self.theme.set(p);
        self.dpi.set(dpi.max(96));
    }

    pub fn attach(self: &Rc<Self>, hwnd: HWND, id: i32) -> Result<()> {
        let face = Rc::new(Face {
            group: self.clone(),
            id,
            hot: Cell::new(false),
            frames: RefCell::new(VecDeque::new()),
        });
        let ptr = Rc::into_raw(face);
        if !unsafe { SetWindowSubclass(hwnd, Some(procedure), SUBCLASS, ptr as usize) }.as_bool() {
            unsafe {
                drop(Rc::from_raw(ptr));
            }
            anyhow::bail!("无法初始化快速搜索按钮");
        }
        Ok(())
    }

    pub fn select(&self, parent: HWND, index: usize) -> bool {
        let previous = self.selected.replace(index);
        if previous == index {
            return false;
        }
        // Prepare both final faces before exposing either. A cold font or theme
        // lookup must not leave a frame with the old choice already cleared.
        let buttons = [index, previous]
            .map(|i| unsafe { GetDlgItem(Some(parent), FIRST_FILTER + i as i32) }.ok());
        for hwnd in buttons.iter().flatten().copied() {
            if let Some(face) = bound_face(hwnd) {
                let mut rect = RECT::default();
                if unsafe { GetClientRect(hwnd, &mut rect) }.is_ok() {
                    face.paint(hwnd, None, rect);
                }
            }
        }
        for hwnd in buttons.into_iter().flatten() {
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
                let _ = UpdateWindow(hwnd);
            }
        }
        true
    }
}

#[derive(PartialEq)]
struct FrameKey {
    label: String,
    palette: Palette,
    dpi: u32,
    width: i32,
    height: i32,
    checked: bool,
    enabled: bool,
    hot: bool,
    pressed: bool,
    focus: bool,
    contrast: bool,
}
struct Face {
    group: Rc<Buttons>,
    id: i32,
    hot: Cell<bool>,
    frames: RefCell<VecDeque<(FrameKey, Surface)>>,
}
impl Face {
    fn render(&self, key: &FrameKey, dc: HDC) {
        let p = key.palette;
        let mut colors = search_ui::surface(p);
        if !key.enabled {
            colors.primary_text = colors.disabled_text;
        } else if key.hot || key.pressed {
            let fill = composite(if key.pressed { p.pressed } else { p.hover }, p.layer).0;
            colors.control = zsui::Color::rgb(fill as u8, (fill >> 8) as u8, (fill >> 16) as u8);
        }
        let r = RECT {
            left: 0,
            top: 0,
            right: key.width,
            bottom: key.height,
        };
        let mark = unsafe { SaveDC(dc) };
        unsafe {
            SetDCBrushColor(dc, rgb(p.layer));
            FillRect(dc, &r, HBRUSH(GetStockObject(DC_BRUSH).0));
        }
        let bounds = Rect {
            x: 1,
            y: 1,
            width: (key.width - 2).max(1),
            height: (key.height - 2).max(1),
        };
        let dpi = Dpi::new(key.dpi as f32);
        let plan = if self.id >= FIRST_FILTER {
            let geometry = zsui::zs_toggle_button_render_plan(
                bounds,
                key.checked,
                zsui::ZsPlatformStyle::current(),
                dpi,
            );
            let mut plan = zsui::zs_toggle_button_native_draw_plan(&geometry, &key.label);
            for command in &mut plan.commands {
                if let zsui::NativeDrawCommand::Text(label) = command {
                    let inset = (2 * key.dpi / 96) as i32;
                    label.style.horizontal_align = zsui::HorizontalAlign::Center;
                    label.bounds.x = bounds.x + inset;
                    label.bounds.width = (bounds.width - 2 * inset).max(1);
                }
            }
            plan
        } else {
            let mut button = zsui::button::<()>(&key.label).enabled(key.enabled);
            button.layout(&mut ViewLayoutCx::new(bounds, dpi));
            let mut cx = ViewPaintCx::new(dpi);
            button.paint(&mut cx);
            cx.into_plan()
        };
        WindowsGdiDrawSink::with_palette_contrast_and_dpi(dc.0, colors, key.contrast, dpi)
            .draw_native_plan(&plan);
        if key.focus {
            let mut focus = r;
            let inset = (3 * key.dpi / 96) as i32;
            unsafe {
                let _ = InflateRect(&mut focus, -inset, -inset);
                let _ = DrawFocusRect(dc, &focus);
            }
        }
        unsafe {
            let _ = RestoreDC(dc, mark);
        }
    }

    fn paint(&self, hwnd: HWND, target: Option<HDC>, r: RECT) {
        let width = r.right - r.left;
        let height = r.bottom - r.top;
        if width <= 0 || height <= 0 {
            return;
        }
        let enabled = unsafe { IsWindowEnabled(hwnd) }.as_bool();
        let key = FrameKey {
            label: text(hwnd),
            palette: self.group.theme.get(),
            dpi: self.group.dpi.get(),
            width,
            height,
            checked: self.id == FIRST_FILTER + self.group.selected.get() as i32,
            enabled,
            hot: enabled && self.hot.get(),
            pressed: enabled
                && unsafe { SendMessageW(hwnd, BM_GETSTATE, None, None) }.0 as u32 & BST_PUSHED
                    != 0,
            focus: enabled
                && unsafe { GetFocus() } == hwnd
                && unsafe { SendMessageW(hwnd, WM_QUERYUISTATE, None, None) }.0 as u32
                    & UISF_HIDEFOCUS
                    == 0,
            contrast: zsdesk_windows::theme::high_contrast(),
        };
        let blit = |s: &Surface| {
            if let Some(target) = target {
                unsafe {
                    let _ = BitBlt(
                        target,
                        r.left,
                        r.top,
                        width,
                        height,
                        Some(s.dc),
                        0,
                        0,
                        SRCCOPY,
                    );
                }
            }
        };
        {
            let frames = self.frames.borrow();
            if let Some((_, s)) = frames.iter().find(|(k, _)| *k == key) {
                blit(s);
                return;
            }
        }
        if let Some(frame) = Surface::new(width, height) {
            self.render(&key, frame.dc);
            blit(&frame);
            let mut frames = self.frames.borrow_mut();
            frames.retain(|(k, _)| {
                k.width == width
                    && k.height == height
                    && k.dpi == key.dpi
                    && k.palette == key.palette
                    && k.contrast == key.contrast
                    && k.label == key.label
            });
            if frames.len() >= 4 {
                frames.pop_front();
            }
            frames.push_back((key, frame));
        } else if let Some(target) = target {
            let band = windows_sys::Win32::Foundation::RECT {
                left: r.left,
                top: r.top,
                right: r.right,
                bottom: r.bottom,
            };
            let buffer = unsafe { zsui::WindowsBufferedPaint::begin(target.0, &band) };
            self.render(&key, buffer.as_ref().map_or(target, |b| HDC(b.hdc())));
        }
    }
}

fn bound_face(hwnd: HWND) -> Option<Rc<Face>> {
    let mut ptr = 0;
    if !unsafe { GetWindowSubclass(hwnd, Some(procedure), SUBCLASS, Some(&mut ptr)) }.as_bool() {
        return None;
    }
    unsafe {
        Rc::increment_strong_count(ptr as *const Face);
        Some(Rc::from_raw(ptr as *const Face))
    }
}
pub(super) fn owner_draw(item: &DRAWITEMSTRUCT) -> bool {
    if item.CtlType != ODT_BUTTON || !owns(item.CtlID as i32) {
        return false;
    }
    let Some(face) = bound_face(item.hwndItem) else {
        return false;
    };
    face.paint(item.hwndItem, Some(item.hDC), item.rcItem);
    true
}
unsafe extern "system" fn procedure(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    let ptr = data as *const Face;
    if msg == WM_NCDESTROY {
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(procedure), id);
            drop(Rc::from_raw(ptr));
        }
        return unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
    }
    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }
    if matches!(msg, WM_PAINT | WM_PRINTCLIENT) {
        unsafe {
            Rc::increment_strong_count(ptr);
        }
        let face = unsafe { Rc::from_raw(ptr) };
        let mut r = RECT::default();
        let _ = unsafe { GetClientRect(hwnd, &mut r) };
        if msg == WM_PRINTCLIENT {
            face.paint(hwnd, Some(HDC(wp.0 as _)), r);
        } else {
            let mut ps = PAINTSTRUCT::default();
            let dc = unsafe { BeginPaint(hwnd, &mut ps) };
            face.paint(hwnd, Some(dc), r);
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
        }
        return LRESULT(0);
    }
    if matches!(msg, WM_MOUSEMOVE | WM_MOUSELEAVE) {
        let hot = msg == WM_MOUSEMOVE;
        if unsafe { &*ptr }.hot.replace(hot) != hot {
            if hot {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    ..Default::default()
                };
                unsafe {
                    let _ = TrackMouseEvent(&mut track);
                }
            }
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}
