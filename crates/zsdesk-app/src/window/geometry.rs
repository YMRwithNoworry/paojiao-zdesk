//! Geometry shared by custom-frame message handling and local regressions.
use windows::Win32::{Foundation::{POINT, RECT}, UI::WindowsAndMessaging::{
    HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT,
    SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED, SW_MINIMIZE, SW_SHOWMINNOACTIVE, WPF_RESTORETOMAXIMIZED,
}};

pub(super) fn resize_hit(window: RECT, point: POINT, border: (i32, i32), maximized: bool, resizable: bool) -> Option<u32> {
    if maximized || !resizable || window.right <= window.left || window.bottom <= window.top
        || point.x < window.left || point.x >= window.right || point.y < window.top || point.y >= window.bottom { return None; }
    let x = border.0.max(1).min((window.right - window.left) / 2);
    let y = border.1.max(1).min((window.bottom - window.top) / 2);
    let left = point.x < window.left + x;
    let right = point.x >= window.right - x;
    let top = point.y < window.top + y;
    let bottom = point.y >= window.bottom - y;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(HTTOPLEFT), (_, true, true, _) => Some(HTTOPRIGHT),
        (true, _, _, true) => Some(HTBOTTOMLEFT), (_, true, _, true) => Some(HTBOTTOMRIGHT),
        (true, _, _, _) => Some(HTLEFT), (_, true, _, _) => Some(HTRIGHT),
        (_, _, true, _) => Some(HTTOP), (_, _, _, true) => Some(HTBOTTOM), _ => None,
    }
}

pub(super) fn is_resize_hit(value: u32) -> bool {
    [HTLEFT,HTRIGHT,HTTOP,HTBOTTOM,HTTOPLEFT,HTTOPRIGHT,HTBOTTOMLEFT,HTBOTTOMRIGHT].contains(&value)
}

/// Preserve DWM's per-button identity. The screen-edge extension only fills
/// pixels DWM did not identify as an actual caption button.
pub(super) fn native_caption_hit(native:isize,edge_padding:Option<u32>)->isize{
    use windows::Win32::UI::WindowsAndMessaging::{HTMINBUTTON,HTMAXBUTTON,HTCLOSE};
    if [HTMINBUTTON,HTMAXBUTTON,HTCLOSE].contains(&(native as u32)){native}
    else{edge_padding.map_or(native,|hit|hit as isize)}
}

pub(super) fn screen_edge_caption_buttons(mut buttons:[zsdesk_ui::chrome::Rect;3],width:i32)->[zsdesk_ui::chrome::Rect;3]{
    for rect in &mut buttons{let bottom=rect.bottom();rect.y=0;rect.height=bottom.max(0);}
    buttons[2].width=(width-buttons[2].x).max(0);buttons
}

pub(super) fn captionless_client(window: RECT, mut native_client: RECT, maximized: bool, frame_y: i32) -> RECT {
    native_client.top = window.top.saturating_add(if maximized { frame_y.max(0) } else { 0 }).min(native_client.bottom);
    native_client
}

pub(super) fn restore_maximized(show: u32, flags: u32) -> bool {
    show == SW_SHOWMAXIMIZED.0 as u32 ||
        ([SW_SHOWMINIMIZED, SW_MINIMIZE, SW_SHOWMINNOACTIVE].iter().any(|value| show == value.0 as u32)
            && flags & WPF_RESTORETOMAXIMIZED.0 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]fn native_button_identity_wins_over_an_inexact_aggregate_rectangle(){
        use windows::Win32::UI::WindowsAndMessaging::{HTMINBUTTON,HTMAXBUTTON,HTCLOSE,HTCAPTION,HTCLIENT,HTBORDER};
        for actual in [HTMINBUTTON,HTMAXBUTTON,HTCLOSE]{
            for fallback in [None,Some(HTMINBUTTON),Some(HTMAXBUTTON),Some(HTCLOSE)]{
                assert_eq!(native_caption_hit(actual as isize,fallback),actual as isize);
            }
        }
        for empty in [HTCLIENT,HTCAPTION,HTBORDER,HTRIGHT]{
            assert_eq!(native_caption_hit(empty as isize,None),empty as isize);
            assert_eq!(native_caption_hit(empty as isize,Some(HTCLOSE)),HTCLOSE as isize);
        }
    }
    #[test]
    fn all_edges_and_corners_resize_at_each_dpi_and_negative_origin() {
        for dpi in [96, 120, 144, 192] {
            let border = 8 * dpi / 96;
            let r = RECT { left: -1600, top: -120, right: -600, bottom: 580 };
            for (point, expected) in [
                (POINT{x:-1599,y:-119}, HTTOPLEFT), (POINT{x:-601,y:-119}, HTTOPRIGHT),
                (POINT{x:-1599,y:579}, HTBOTTOMLEFT), (POINT{x:-601,y:579}, HTBOTTOMRIGHT),
                (POINT{x:-1599,y:230}, HTLEFT), (POINT{x:-601,y:230}, HTRIGHT),
                (POINT{x:-1100,y:-119}, HTTOP), (POINT{x:-1100,y:579}, HTBOTTOM),
            ] { assert_eq!(resize_hit(r, point, (border,border), false, true), Some(expected)); }
            assert_eq!(resize_hit(r, POINT{x:-1100,y:230}, (border,border), false, true), None);
            assert_eq!(resize_hit(r, POINT{x:-601,y:230}, (border,border), true, true), None);
            assert_eq!(resize_hit(r, POINT{x:-601,y:230}, (border,border), false, false), None);
            assert_eq!(resize_hit(r, POINT{x:-600,y:230}, (border,border), false, true), None);
        }
    }
    #[test]
    fn restore_does_not_accumulate_the_maximized_top_inset() {
        let normal = RECT { left: 100, top: 200, right: 1100, bottom: 900 };
        let normal_client = RECT { left: 108, top: 231, right: 1092, bottom: 892 };
        let restored = captionless_client(normal, normal_client, false, 8);
        assert_eq!((restored.left, restored.top, restored.right, restored.bottom), (108,200,1092,892));
        let zoomed = captionless_client(RECT { left:-8,top:-8,right:1928,bottom:1048 },
            RECT { left:0,top:23,right:1920,bottom:1040 }, true, 8);
        assert_eq!((zoomed.left,zoomed.top,zoomed.right,zoomed.bottom),(0,0,1920,1040));
        assert_eq!(captionless_client(normal, normal_client, false, 8), restored);
    }
    #[test]
    fn closing_a_minimized_maximized_window_keeps_its_restore_state() {
        assert!(restore_maximized(SW_SHOWMAXIMIZED.0 as u32, 0));
        assert!(restore_maximized(SW_SHOWMINIMIZED.0 as u32, WPF_RESTORETOMAXIMIZED.0));
        assert!(!restore_maximized(SW_SHOWMINIMIZED.0 as u32, 0));
        assert!(!restore_maximized(1, WPF_RESTORETOMAXIMIZED.0));
    }

    #[test]
    fn maximized_caption_targets_reach_the_top_and_right_without_overlap(){
        use zsdesk_ui::chrome::{Rect,scale};
        for dpi in [96,120,144,192]{
            let width=scale(1200,dpi);let button=scale(46,dpi);let inset=scale(2,dpi);
            let drawn=[0,1,2].map(|index|Rect::new(width-inset-button*(3-index),inset,button,scale(32,dpi)));
            let hit=screen_edge_caption_buttons(drawn,width);
            assert!(hit[2].contains(width-1,0));
            assert!(!hit[2].contains(width,0));assert!(!hit[2].contains(width-1,-1));
            for index in 0..3{
                assert!(hit[index].contains(drawn[index].x,0));
                assert_eq!(hit[index].bottom(),drawn[index].bottom());
                if index<2{assert_eq!(hit[index].right(),hit[index+1].x);}
            }
        }
    }
}
