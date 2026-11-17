//! Side-tab resizing and scrolling never select or navigate a tab.
use super::*;

impl WindowState{
    fn scroll_side_tabs_to(&mut self,first:usize){
        if !self.layout.vertical_tabs{return;}
        self.layout.set_tab_viewport(self.session.tabs.len(),first);
        self.side_tab_first=self.layout.tabs.first().map(|tab|tab.index);
        self.set_hover(Hit::None,None);self.invalidate_rect(self.layout.tab_panel);
    }
    pub(super) fn side_tabs_mouse_down(&mut self,hit:Hit,point:POINT)->bool{
        match hit{
            Hit::TabSplitter=>{self.side_tab_resize=Some((point.x,self.layout.tab_panel.width));unsafe{SetCapture(self.hwnd);}},
            Hit::TabScrollbar=>{
                let first=self.layout.tabs.first().map_or(0,|tab|tab.index);
                if self.layout.tab_scroll_thumb.contains(point.x,point.y){self.side_tab_scroll_drag=Some((point.y,first));unsafe{SetCapture(self.hwnd);}}
                else{let page=self.layout.tabs.len().saturating_sub(1).max(1);self.scroll_side_tabs_to(if point.y<self.layout.tab_scroll_thumb.y{first.saturating_sub(page)}else{first.saturating_add(page)});}
            },_=>return false,
        }
        true
    }
    pub(super) fn side_tabs_mouse_move(&mut self,point:POINT)->bool{
        if let Some((start,width))=self.side_tab_resize{
            let direction=if self.session.ui.tab_placement==zsdesk_core::model::explorer::TabPlacement::Left{1}else{-1};
            let maximum=(self.client_size().0-self.s(440)).max(self.s(120)).min(self.s(420));
            let pixels=(width+direction*(point.x-start)).clamp(self.s(120),maximum);
            let dip=((pixels as i64*96+self.dpi as i64/2)/self.dpi.max(96)as i64)as i32;
            if self.session.ui.side_tabs_width!=dip{self.session.ui.side_tabs_width=dip;self.relayout();self.position_children();self.invalidate();}
            return true;
        }
        if let Some((start,first))=self.side_tab_scroll_drag{
            let travel=self.layout.tab_scroll_track.height-self.layout.tab_scroll_thumb.height;
            let limit=self.session.tabs.len().saturating_sub(self.layout.tabs.len());
            if travel>0{let next=(first as i64+(point.y-start)as i64*limit as i64/travel as i64).clamp(0,limit as i64)as usize;self.scroll_side_tabs_to(next);}
            return true;
        }
        false
    }
    pub(super) fn side_tabs_mouse_up(&mut self,had_capture:bool)->bool{
        let resized=self.side_tab_resize.take().is_some();let scrolled=self.side_tab_scroll_drag.take().is_some();
        if !resized&&!scrolled{return false;}
        if had_capture{let _=unsafe{ReleaseCapture()};}
        if resized{self.save();}self.invalidate_rect(self.layout.tab_panel);true
    }
    pub(super) fn side_tabs_wheel(&mut self,message:&MSG)->bool{
        if message.message!=WM_MOUSEWHEEL||!self.layout.vertical_tabs||message.wParam.0&8!=0||unsafe{GetKeyState(VK_CONTROL.0 as i32)}<0{return false;}
        let mut point=POINT{x:x_lparam(message.lParam.0),y:y_lparam(message.lParam.0)};
        if !unsafe{ScreenToClient(self.hwnd,&mut point)}.as_bool()||!self.layout.tab_panel.contains(point.x,point.y){return false;}
        self.side_tab_wheel=self.side_tab_wheel.saturating_add(((message.wParam.0>>16)as i16)as i32);
        let steps=(self.side_tab_wheel/120).clamp(-16,16);self.side_tab_wheel%=120;
        if steps!=0{let first=self.layout.tabs.first().map_or(0,|tab|tab.index);let next=(first as i64-i64::from(steps)*3).max(0)as usize;self.scroll_side_tabs_to(next);}
        true
    }
}
