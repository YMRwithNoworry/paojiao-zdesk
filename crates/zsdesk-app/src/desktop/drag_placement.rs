//! Immutable drag geometry in DIP; placements are relative to the destination work area.
use zsdesk_core::model::desktop::Bounds;

pub(super) fn signed_scale(value:i32,numerator:u32,denominator:u32)->i32{
    let value=i64::from(value)*i64::from(numerator);
    ((value+value.signum()*i64::from(denominator/2))/i64::from(denominator.max(1))) as i32
}

/// Only rectangles that are actually visible on this work area can displace a
/// drop. Saved positions for absent files are recovery data, not obstacles.
pub(super) fn visible_obstacle(rect:Bounds,work:Bounds,dpi:u32,visible:bool,dragged:bool)->Option<Bounds>{
    if !visible||dragged{return None;}
    let left=rect.x.max(work.x);let top=rect.y.max(work.y);
    let right=rect.right().min(work.right());let bottom=rect.bottom().min(work.bottom());
    if left>=right||top>=bottom{return None;}
    let x=signed_scale(left-work.x,96,dpi);let y=signed_scale(top-work.y,96,dpi);
    Some(Bounds::new(x,y,(signed_scale(right-work.x,96,dpi)-x).max(1),(signed_scale(bottom-work.y,96,dpi)-y).max(1)))
}

/// A dense fence/list becomes desktop cells; keep the grabbed item under the
/// same pointer hot spot instead of always anchoring the first selected item.
pub(super) fn incoming_offsets(count:usize,grabbed:usize,hot_spot:(i32,i32),cell:(i32,i32))->Vec<Bounds>{
    let columns=count.min(8).max(1)as i32;let grabbed=grabbed.min(count.saturating_sub(1))as i32;
    (0..count).map(|index|{
        let index=index as i32;
        Bounds::new((index%columns-grabbed%columns)*cell.0-hot_spot.0,(index/columns-grabbed/columns)*cell.1-hot_spot.1,cell.0,cell.1)
    }).collect()
}

/// Translate the whole selection once. Never repack individual selected icons.
pub(super) fn place(offsets:&[Bounds],pointer:(i32,i32),area:(i32,i32),occupied:&[Bounds])->Option<Vec<Bounds>>{
    if offsets.is_empty(){return Some(Vec::new());}
    if offsets.iter().any(|b|b.w<=0||b.h<=0||b.w>area.0||b.h>area.1){return None;}
    let left=offsets.iter().map(|b|b.x).min().unwrap();let top=offsets.iter().map(|b|b.y).min().unwrap();
    let right=offsets.iter().map(Bounds::right).max().unwrap();let bottom=offsets.iter().map(Bounds::bottom).max().unwrap();
    let clamp=|x:i32,y:i32| (x.clamp(-left,(area.0-right).max(-left)),y.clamp(-top,(area.1-bottom).max(-top)));
    let origin=clamp(pointer.0,pointer.1);
    let overlap=|x:i32,y:i32| offsets.iter().any(|b|occupied.iter().any(|o|b.x+x<o.right()&&b.right()+x>o.x&&b.y+y<o.bottom()&&b.bottom()+y>o.y));
    let fits=right-left<=area.0&&bottom-top<=area.1;
    let mut best=origin;let mut distance=i64::MAX;let mut found=fits&&!overlap(origin.0,origin.1);
    if fits&&!found{
        // Candidate translations follow obstacle boundaries, ordered by distance
        // to the requested drop. No desktop-wide jump to the first grid cell.
        let mut xs=vec![origin.0,-left,(area.0-right).max(-left)];let mut ys=vec![origin.1,-top,(area.1-bottom).max(-top)];
        for o in occupied.iter().take(512){xs.extend([o.x-right,o.right()-left]);ys.extend([o.y-bottom,o.bottom()-top]);}
        xs.sort_unstable();xs.dedup();ys.sort_unstable();ys.dedup();
        // Most collisions resolve by moving along one axis. Bound candidate
        // count for large desktops; preserve relative offsets even when full.
        let mut candidates:Vec<_>=xs.iter().map(|x|clamp(*x,origin.1)).chain(ys.iter().map(|y|clamp(origin.0,*y))).collect();
        let near=|values:&mut Vec<i32>,at:i32|{values.sort_by_key(|v|(i64::from(*v)-i64::from(at)).abs());values.truncate(16);};
        near(&mut xs,origin.0);near(&mut ys,origin.1);
        for x in xs{for y in &ys{candidates.push(clamp(x,*y));}}
        for (x,y) in candidates{let dx=i64::from(x)-i64::from(origin.0);let dy=i64::from(y)-i64::from(origin.1);let d=dx*dx+dy*dy;
            if d<distance&&!overlap(x,y){best=(x,y);distance=d;found=true;}}
    }
    if found{return Some(offsets.iter().map(|b|Bounds::new(b.x+best.0,b.y+best.1,b.w,b.h)).collect());}
    // A tall/scattered selection may not fit as a rigid group even when all
    // its items fit. Reflow in reading order instead of clipping or stacking.
    let width=offsets.iter().map(|b|b.w).max()?;let height=offsets.iter().map(|b|b.h).max()?;
    let columns=area.0/width;let rows=area.1/height;
    if i64::from(columns)*i64::from(rows)>16384{return None;}
    let mut cells=Vec::new();
    for row in 0..rows{for column in 0..columns{
        let cell=Bounds::new(column*width,row*height,width,height);
        if occupied.iter().all(|o|cell.right()<=o.x||cell.x>=o.right()||cell.bottom()<=o.y||cell.y>=o.bottom()){cells.push(cell);}
    }}
    if cells.len()<offsets.len(){return None;}
    cells.sort_by_key(|b|{let dx=i64::from(b.x+width/2)-i64::from(pointer.0);let dy=i64::from(b.y+height/2)-i64::from(pointer.1);(dx*dx+dy*dy,b.y,b.x)});
    cells.truncate(offsets.len());cells.sort_by_key(|b|(b.y,b.x));
    let mut order:Vec<_>=(0..offsets.len()).collect();order.sort_by_key(|i|(offsets[*i].y,offsets[*i].x,*i));
    let mut result=vec![Bounds::default();offsets.len()];
    for (index,cell) in order.into_iter().zip(cells){result[index]=Bounds::new(cell.x,cell.y,offsets[index].w,offsets[index].h);}
    Some(result)
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn an_absent_saved_icon_cannot_push_the_drop_away_from_the_pointer(){
        let work=Bounds::new(0,0,2560,1440);let stale=Bounds::new(86,146,96,104);
        let occupied:Vec<_>=visible_obstacle(stale,work,96,false,false).into_iter().collect();
        let offsets=[Bounds::new(-28,-20,96,104)];
        assert_eq!(place(&offsets,(120,180),(2560,1440),&occupied).unwrap()[0],Bounds::new(92,160,96,104));
        assert_ne!(place(&offsets,(120,180),(2560,1440),&[stale]).unwrap()[0],Bounds::new(92,160,96,104));
        assert!(visible_obstacle(stale,work,96,true,true).is_none());
    }
    #[test]fn obstacle_coordinates_use_the_destination_work_area_and_visible_intersection(){
        let work=Bounds::new(-2560,40,2560,1400);
        assert_eq!(visible_obstacle(Bounds::new(-2600,20,160,180),work,144,true,false),Some(Bounds::new(0,0,80,107)));
        assert!(visible_obstacle(Bounds::new(0,100,96,104),work,144,true,false).is_none());
    }
    #[test]fn dragging_a_later_selected_fence_item_preserves_its_hot_spot(){
        for grabbed in 0..4{
            let offsets=incoming_offsets(4,grabbed,(29,37),(96,104));
            let placed=place(&offsets,(900,600),(2560,1440),&[]).unwrap();
            assert_eq!(placed[grabbed],Bounds::new(871,563,96,104));
            for(index,b)in placed.iter().enumerate(){assert!(placed[..index].iter().all(|a|a.right()<=b.x||a.x>=b.right()||a.bottom()<=b.y||a.y>=b.bottom()));}
        }
    }
    #[test]fn hot_spot_and_multi_selection_offsets_survive_moves_and_monitor_scaling(){
        for dpi in [96,120,144,192]{
            let offsets=vec![Bounds::new(-24,-30,96,104),Bounds::new(146,70,96,104)];
            let work_left=-2560;let work_top=40;
            let drop=(work_left+signed_scale(500,dpi,96),work_top+signed_scale(400,dpi,96));
            let p=place(&offsets,(signed_scale(drop.0-work_left,96,dpi),signed_scale(drop.1-work_top,96,dpi)),(1600,900),&[]).unwrap();
            assert_eq!(p[0],Bounds::new(476,370,96,104));assert_eq!((p[1].x-p[0].x,p[1].y-p[0].y),(170,100));
            assert_eq!(signed_scale(-30,dpi,96),-signed_scale(30,dpi,96));
        }
    }
    #[test]fn collision_uses_nearby_space_and_edges_translate_selection_together(){
        let offsets=[Bounds::new(-20,-20,96,104),Bounds::new(100,80,96,104)];
        let p=place(&offsets,(700,400),(1600,900),&[Bounds::new(680,380,96,104)]).unwrap();
        assert!(p[0].x>400&&p[0].y>150,"collision teleported to top left");
        assert_eq!((p[1].x-p[0].x,p[1].y-p[0].y),(120,100));
        let edge=place(&offsets,(1600,900),(1600,900),&[]).unwrap();
        assert_eq!(edge[1].right(),1600);assert_eq!(edge[1].bottom(),900);
        assert_eq!((edge[1].x-edge[0].x,edge[1].y-edge[0].y),(120,100));
    }
    #[test]fn oversized_groups_reflow_without_hidden_or_overlapping_icons(){
        let offsets:Vec<_>=(0..160).map(|i|Bounds::new((i%8)*96,(i/8)*104,96,104)).collect();
        let positions=place(&offsets,(1200,700),(2560,1560),&[Bounds::new(0,0,2560,40)]).unwrap();
        assert_eq!(positions.len(),160);
        for (index,b) in positions.iter().enumerate(){
            assert!(b.x>=0&&b.y>=40&&b.right()<=2560&&b.bottom()<=1560);
            assert!(positions[..index].iter().all(|a|a.right()<=b.x||a.x>=b.right()||a.bottom()<=b.y||a.y>=b.bottom()));
        }
        assert!(place(&offsets,(100,100),(800,600),&[]).is_none());
        assert!(place(&[Bounds::new(0,0,96,104)],(100,100),(800,600),&[Bounds::new(0,0,800,600)]).is_none());
    }
}
