use super::*;

pub(super) struct InfoRow{pub section:bool,pub key:String,pub description:String}
pub(super) fn shortcut_rows(body:&str)->Vec<InfoRow>{
    let mut rows=Vec::new();
    for line in body.lines().map(str::trim).filter(|line|!line.is_empty()){
        if ["文件管理器","桌面格子","全局搜索"].iter().any(|title|line==*title||line==zsdesk_core::i18n::text(title)){
            rows.push(InfoRow{section:true,key:String::new(),description:line.into()});continue;
        }
        for part in line.split('　').map(str::trim).filter(|part|!part.is_empty()){
            let chinese=part.char_indices().find(|(_,c)|('\u{3400}'..='\u{9fff}').contains(c)).map(|(index,_)|index);
            let pair=match chinese{Some(index)if index>0=>Some((&part[..index],&part[index..])),Some(0)=>part.split_once(' '),_=>part.split_once("  ")};
            let(key,description)=pair.unwrap_or(("",part));
            rows.push(InfoRow{section:false,key:key.trim().into(),description:description.trim().into()});
        }
    }rows
}
pub(super) fn rounded(dc:HDC,rect:RECT,fill:COLORREF,border:COLORREF,radius:i32){
    let brush=unsafe{CreateSolidBrush(fill)};let pen=unsafe{CreatePen(PS_SOLID,1,border)};
    let old_brush=unsafe{SelectObject(dc,brush.into())};let old_pen=unsafe{SelectObject(dc,pen.into())};
    unsafe{let _=RoundRect(dc,rect.left,rect.top,rect.right,rect.bottom,radius,radius);SelectObject(dc,old_pen);SelectObject(dc,old_brush);
        let _=DeleteObject(pen.into());let _=DeleteObject(brush.into());}
}
fn wrapped(dc:HDC,text:&str,rect:RECT,font:HFONT,color:COLORREF){
    let old=unsafe{SelectObject(dc,font.into())};let mut area=rect;let mut text:Vec<_>=text.encode_utf16().collect();
    unsafe{SetBkMode(dc,TRANSPARENT);SetTextColor(dc,color);DrawTextW(dc,&mut text,&mut area,DT_WORDBREAK|DT_NOPREFIX|DT_END_ELLIPSIS);SelectObject(dc,old);}
}
impl Panel{
    fn row_height(&self,row:&InfoRow)->i32{self.s(if row.section{38}else if row.key.is_empty(){62}else{48})}
    fn content_height(&self)->i32{self.info.iter().map(|row|self.row_height(row)).sum::<i32>()+self.s(12)}
    pub(super) fn update_scroll(&mut self){
        if self.kind!=Kind::Shortcuts{return;}
        let mut rect=RECT::default();let _=unsafe{GetClientRect(self.edit,&mut rect)};
        self.scroll=self.scroll.clamp(0,(self.content_height()-rect.bottom).max(0));
        let info=SCROLLINFO{cbSize:std::mem::size_of::<SCROLLINFO>()as u32,fMask:SIF_RANGE|SIF_PAGE|SIF_POS,nMin:0,
            nMax:self.content_height().saturating_sub(1),nPage:rect.bottom.max(1)as u32,nPos:self.scroll,..Default::default()};
        unsafe{SetScrollInfo(self.edit,SB_VERT,&info,true);}
    }
    pub(super) fn scroll_info(&mut self,code:u32,wheel:i32){
        if self.kind!=Kind::Shortcuts{return;}
        let mut rect=RECT::default();let _=unsafe{GetClientRect(self.edit,&mut rect)};
        let mut info=SCROLLINFO{cbSize:std::mem::size_of::<SCROLLINFO>()as u32,fMask:SIF_TRACKPOS,..Default::default()};
        let _=unsafe{GetScrollInfo(self.edit,SB_VERT,&mut info)};
        let max=(self.content_height()-rect.bottom).max(0);
        let next=if wheel!=0{self.scroll-wheel*self.s(120)/120}else{match code{
            0=>self.scroll-self.s(48),1=>self.scroll+self.s(48),2=>self.scroll-rect.bottom,3=>self.scroll+rect.bottom,
            4|5=>info.nTrackPos,6=>0,7=>max,_=>self.scroll,
        }};
        self.scroll=next.clamp(0,max);self.update_scroll();let _=unsafe{InvalidateRect(Some(self.edit),None,false)};
    }
    pub(super) fn paint_info(&self,dc:HDC,rect:RECT){
        unsafe{FillRect(dc,&rect,self.brush);}
        let p=&self.palette;let text=composite(p.text,p.layer);let secondary=composite(p.text_secondary,p.layer);
        if self.kind==Kind::About{
            if let Ok(module)=unsafe{GetModuleHandleW(None)}{
                if let Ok(icon)=unsafe{LoadIconW(Some(HINSTANCE(module.0)),PCWSTR(101usize as*const u16))}{
                    let _=unsafe{DrawIconEx(dc,self.s(4),self.s(8),icon,self.s(56),self.s(56),0,None,DI_NORMAL)};
                }
            }
            draw_text(dc,"ZSDesk",RECT{left:self.s(80),top:self.s(4),right:rect.right,bottom:self.s(42)},self.title_font,text);
            draw_text(dc,&format!("{} · {}",env!("CARGO_PKG_VERSION"),crate::edition::name()),RECT{left:self.s(80),top:self.s(42),right:rect.right,bottom:self.s(70)},self.font,secondary);
            let card=RECT{left:0,top:self.s(90),right:rect.right,bottom:(self.s(202)).min(rect.bottom)};
            rounded(dc,card,rgb(p.input),composite(p.divider,p.input),self.s(10));
            wrapped(dc,zsdesk_core::i18n::text("多标签文件管理器与桌面整理"),RECT{left:self.s(18),top:self.s(106),right:rect.right-self.s(18),bottom:self.s(138)},self.font,composite(p.text,p.input));
            wrapped(dc,zsdesk_core::i18n::text("文件浏览、快速搜索、内容预览与桌面格子。"),RECT{left:self.s(18),top:self.s(144),right:rect.right-self.s(18),bottom:card.bottom-self.s(10)},self.font,composite(p.text_secondary,p.input));
            return;
        }
        let saved=unsafe{SaveDC(dc)};unsafe{IntersectClipRect(dc,rect.left,rect.top,rect.right,rect.bottom);}
        let mut y=self.s(4)-self.scroll;
        for row in &self.info{
            let height=self.row_height(row);let bottom=y+height;
            if bottom>=0&&y<rect.bottom{
                if row.section{
                    draw_text(dc,&row.description,RECT{left:self.s(4),top:y,right:rect.right,bottom},self.font,secondary);
                }else{
                    let card=RECT{left:0,top:y,right:rect.right-self.s(4),bottom:bottom-self.s(3)};
                    rounded(dc,card,rgb(p.input),composite(p.divider,p.input),self.s(8));
                    let key_width=self.s(252).min((rect.right/2).max(self.s(120)));
                    let key_left=rect.right-key_width-self.s(16);
                    let description=RECT{left:self.s(16),top:y+self.s(10),right:if row.key.is_empty(){card.right-self.s(16)}else{key_left-self.s(14)},bottom:bottom-self.s(6)};
                    wrapped(dc,&row.description,description,self.font,composite(p.text,p.input));
                    if !row.key.is_empty(){
                        let key=RECT{left:key_left,top:y+self.s(8),right:card.right-self.s(12),bottom:bottom-self.s(10)};
                        rounded(dc,key,composite(p.hover,p.input),composite(p.divider,p.input),self.s(6));
                        draw_text(dc,&row.key,RECT{left:key.left+self.s(10),right:key.right-self.s(6),..key},self.font,composite(p.text,p.input));
                    }
                }
            }y=bottom;
        }
        if saved!=0{unsafe{RestoreDC(dc,saved);}}
    }
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn english_shortcuts_keep_section_and_key_columns(){
        let _language=zsdesk_core::i18n::Scope::english(true);
        let rows=shortcut_rows(crate::window::shortcut_help_text());
        assert_eq!(rows.iter().filter(|row|row.section).count(),if crate::edition::DESKTOP&&!crate::edition::DESKTOP_ONLY{2}else{1});
        if !crate::edition::DESKTOP_ONLY{assert!(rows.iter().any(|row|row.key=="Ctrl+W / Ctrl+F4"&&row.description=="Close tab"));}
        if crate::edition::DESKTOP{assert!(rows.iter().any(|row|row.key=="Hover"&&row.description=="Expand a collapsed fence"));}
    }
    #[test]fn shortcut_aliases_and_groups_keep_their_meaning(){
        let rows=shortcut_rows("文件管理器\nCtrl+W / Ctrl+F4 关闭标签　Ctrl+F1 展开或收起命令栏\n\n桌面格子\n鼠标悬停 展开格子");
        assert_eq!(rows.len(),5);assert!(rows[0].section&&rows[3].section);
        assert_eq!(rows[1].key,"Ctrl+W / Ctrl+F4");assert_eq!(rows[1].description,"关闭标签");
        assert_eq!(rows[4].key,"鼠标悬停");assert_eq!(rows[4].description,"展开格子");
    }
}
