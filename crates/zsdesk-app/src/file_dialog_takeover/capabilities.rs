//! Preserve caller-owned selection actions which a single-action replica cannot represent.
use super::*;

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
enum ActionKind { Files, Folders, Mixed, Other }

fn action_kind(label:&str)->ActionKind {
    let label=control_label(label);
    if ["文件和文件夹","文件或文件夹","文件/文件夹","files and folders","files or folders","files/folders","file or folder"]
        .iter().any(|text|label.contains(text)){return ActionKind::Mixed;}
    if ["选择文件夹","选择此文件夹","选取文件夹","选取目录","选择目录","上传文件夹","添加文件夹",
        "select folder","choose folder","select directory","choose directory","upload folder","add folder"]
        .iter().any(|text|label.starts_with(text)){return ActionKind::Folders;}
    if ["选择文件","选取文件","上传文件","添加文件","select file","choose file","upload file","add file"]
        .iter().any(|text|label.starts_with(text)){return ActionKind::Files;}
    ActionKind::Other
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(super) enum Preservation {
    MixedSelection,
    AdditionalSelectionAction,
    ConfirmationDropdown,
    InspectionUnavailable,
}
impl Preservation {
    pub fn key(self)->&'static str{match self{
        Self::MixedSelection=>"mixed-file-folder-actions",
        Self::AdditionalSelectionAction=>"additional-selection-action",
        Self::ConfirmationDropdown=>"confirmation-dropdown",
        Self::InspectionUnavailable=>"selection-capabilities-unavailable",
    }}
}

#[derive(Clone,Debug,PartialEq,Eq)]
struct Action { kind:ActionKind, primary:bool, dropdown:bool, secondary_confirmation:bool }

fn same_confirmation_row(primary:windows::Win32::Foundation::RECT,other:windows::Win32::Foundation::RECT)->bool{
    let height=i64::from(primary.bottom)-i64::from(primary.top);
    height>0&&primary.right>primary.left&&other.bottom>other.top&&other.right>other.left
        &&(i64::from(other.top)-i64::from(primary.top)).abs()<=height/2&&(i64::from(other.bottom)-i64::from(primary.bottom)).abs()<=height/2
}
fn has_dropdown(split_control:bool,state:Option<i32>)->bool{state.map_or(split_control,|state|state!=ExpandCollapseState_LeafNode.0)}

fn field_root(mut window:HWND)->HWND{
    if let Ok(parent)=unsafe{GetParent(window)}{
        if matches!(class(parent).as_str(),"ComboBox"|"ComboBoxEx32"){window=parent;}
    }window
}
fn inside_field(automation:&IUIAutomation,walker:&IUIAutomationTreeWalker,item:&IUIAutomationElement,fields:&[IUIAutomationElement])->windows::core::Result<bool>{
    let mut current=item.clone();
    for _ in 0..32{
        for field in fields{if unsafe{automation.CompareElements(&current,field)}?.as_bool(){return Ok(true);}}
        let Ok(parent)=(unsafe{walker.GetParentElement(&current)})else{return Ok(false);};current=parent;
    }
    Err(windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32),"选择控件层级超出限制"))
}

fn preserve(primary_kind:Option<SelectionKind>,actions:&[Action])->Option<Preservation>{
    if actions.iter().any(|action|action.primary&&action.dropdown){return Some(Preservation::ConfirmationDropdown);}
    if actions.iter().any(|action|action.kind==ActionKind::Mixed){return Some(Preservation::MixedSelection);}
    let files=primary_kind==Some(SelectionKind::Files)||actions.iter().any(|action|action.kind==ActionKind::Files);
    let folders=primary_kind==Some(SelectionKind::Folders)||actions.iter().any(|action|action.kind==ActionKind::Folders);
    if files&&folders{return Some(Preservation::MixedSelection);}
    actions.iter().any(|action|!action.primary&&(action.secondary_confirmation||matches!(action.kind,ActionKind::Files|ActionKind::Folders)))
        .then_some(Preservation::AdditionalSelectionAction)
}

/// The original dialog remains the authority for every extra button and
/// dropdown callback. Never synthesize an IDOK result for those contracts.
pub(super) fn preservation(dialog:HWND,part:&Parts,automation:Option<&IUIAutomation>)->Option<Preservation>{
    let inspect=(||->windows::core::Result<Option<Preservation>>{
        let primary=unsafe{GetDlgItem(Some(dialog),1)}?;
        let automation=automation.ok_or_else(||windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32),"原窗口选择能力不可读取"))?;
        let root=unsafe{automation.ElementFromHandle(dialog)}?;
        let primary=unsafe{automation.ElementFromHandle(primary)}?;
        let primary_bounds=unsafe{primary.CurrentBoundingRectangle()}?;
        let cancel=unsafe{GetDlgItem(Some(dialog),2)}.and_then(|window|unsafe{automation.ElementFromHandle(window)})?;
        let walker=unsafe{automation.ControlViewWalker()}?;
        let mut fields=Vec::new();
        for window in [part.name,part.types]{if !window.0.is_null(){fields.push(unsafe{automation.ElementFromHandle(field_root(window))}?);}}
        let expansion=unsafe{primary.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId)}.ok()
            .map(|pattern|unsafe{pattern.CurrentExpandCollapseState()}).transpose()?;
        let primary_dropdown=has_dropdown(unsafe{primary.CurrentControlType()}?==UIA_SplitButtonControlTypeId,expansion.map(|state|state.0));
        if primary_dropdown{
            return Ok(Some(Preservation::ConfirmationDropdown));
        }
        let buttons=unsafe{automation.CreatePropertyCondition(UIA_ControlTypePropertyId,&VARIANT::from(UIA_ButtonControlTypeId.0))}?;
        let split=unsafe{automation.CreatePropertyCondition(UIA_ControlTypePropertyId,&VARIANT::from(UIA_SplitButtonControlTypeId.0))}?;
        let menu=unsafe{automation.CreatePropertyCondition(UIA_ControlTypePropertyId,&VARIANT::from(UIA_MenuItemControlTypeId.0))}?;
        let condition=unsafe{automation.CreateOrCondition(&buttons,&split)}?;
        let condition=unsafe{automation.CreateOrCondition(&condition,&menu)}?;
        let elements=unsafe{root.FindAll(TreeScope_Descendants,&condition)}?;
        let count=unsafe{elements.Length()}?;
        if !(0..=256).contains(&count){return Ok(Some(Preservation::InspectionUnavailable));}
        let mut actions=Vec::with_capacity(count as usize);
        for index in 0..count{
            let item=unsafe{elements.GetElement(index)}?;
            if inside_field(automation,&walker,&item,&fields)?{continue;}
            let label=unsafe{item.CurrentName()}?.to_string();
            let is_primary=unsafe{automation.CompareElements(&item,&primary)}?.as_bool();
            if unsafe{automation.CompareElements(&item,&cancel)}?.as_bool(){continue;}
            let bounds=unsafe{item.CurrentBoundingRectangle()}?;
            actions.push(Action{kind:action_kind(&label),primary:is_primary,
                secondary_confirmation:!is_primary&&same_confirmation_row(primary_bounds,bounds),
                dropdown:is_primary&&primary_dropdown});
        }
        if !actions.iter().any(|action|action.primary){return Ok(Some(Preservation::InspectionUnavailable));}
        Ok(preserve(dialog_kind(dialog,part,Some(automation)),&actions))
    })();
    inspect.unwrap_or(Some(Preservation::InspectionUnavailable))
}

#[cfg(test)]mod tests{
    use super::*;
    fn action(label:&str,primary:bool)->Action{Action{kind:action_kind(label),primary,dropdown:false,secondary_confirmation:false}}
    #[test]fn a_file_dialog_with_a_folder_action_keeps_both_native_entry_points(){
        let actions=[action("打开(&O)",true),action("选择文件夹(&D)",false),action("取消",false)];
        assert_eq!(preserve(Some(SelectionKind::Files),&actions),Some(Preservation::MixedSelection));
    }
    #[test]fn folder_primary_and_custom_file_action_are_not_collapsed_into_folder_only(){
        assert_eq!(preserve(Some(SelectionKind::Folders),&[action("选择文件夹",true),action("Select files",false)]),Some(Preservation::MixedSelection));
        assert_eq!(preserve(None,&[action("选择文件或文件夹",true)]),Some(Preservation::MixedSelection));
    }
    #[test]fn confirmation_dropdowns_are_preserved_even_before_their_items_are_visible(){
        let actions=[Action{kind:ActionKind::Files,primary:true,dropdown:true,secondary_confirmation:false}];
        assert_eq!(preserve(Some(SelectionKind::Files),&actions),Some(Preservation::ConfirmationDropdown));
        assert!(has_dropdown(true,None));assert!(has_dropdown(false,Some(ExpandCollapseState_Collapsed.0)));
        assert!(!has_dropdown(true,Some(ExpandCollapseState_LeafNode.0)),"a non-expanding button is not a dropdown merely because its native class supports splitting");
    }
    #[test]fn ordinary_navigation_and_single_mode_dialogs_do_not_gain_folder_submission(){
        let actions=[action("Open",true),action("New folder",false),action("新建文件夹",false),action("文件夹：",false)];
        assert_eq!(preserve(Some(SelectionKind::Files),&actions),None);
        assert_eq!(preserve(Some(SelectionKind::Folders),&[action("选择文件夹",true),action("取消",false)]),None);
        assert_eq!(action_kind("Choose files and folders"),ActionKind::Mixed);
    }
    #[test]fn a_late_or_localized_second_confirmation_cannot_be_treated_as_plain_idok(){
        let primary=action("Open",true);
        assert_eq!(preserve(Some(SelectionKind::Files),&[primary.clone()]),None);
        let mut extra=action("Ordner auswählen",false);extra.secondary_confirmation=true;
        assert_eq!(preserve(Some(SelectionKind::Files),&[primary,extra]),Some(Preservation::AdditionalSelectionAction));
        use windows::Win32::Foundation::RECT;
        let first=RECT{left:200,top:400,right:300,bottom:430};
        assert!(same_confirmation_row(first,RECT{left:80,top:400,right:180,bottom:430}));
        assert!(!same_confirmation_row(first,RECT{left:80,top:20,right:180,bottom:50}));
        assert!(!same_confirmation_row(RECT::default(),RECT::default()));
    }
}
