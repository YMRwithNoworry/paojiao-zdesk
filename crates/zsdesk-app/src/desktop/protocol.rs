//! Private desktop messages share one allocation table, including messages
//! sent to menu workers. Duplicate enum discriminants are compilation errors.
//! Timer IDs are scoped to their receiving HWND, as required by SetTimer.
use windows::Win32::UI::WindowsAndMessaging::WM_APP;

#[repr(u32)]
pub(super) enum Message{
    Tray=WM_APP+10,
    DesktopChanged=WM_APP+11,
    FolderChanged=WM_APP+12,
    IconReady=WM_APP+13,
    DesktopDoubleClick=WM_APP+14,
    RenameDone=WM_APP+15,
    CommandRequest=WM_APP+20,
    RenameRequest=WM_APP+21,
    Foreground=WM_APP+22,
    RepairLayer=WM_APP+23,
    MenuReady=WM_APP+24,
    MenuCancel=WM_APP+25,
    OutsideSelection=WM_APP+26,
    ExternalDrop=WM_APP+27,
    MouseInput=WM_APP+28,
}
#[repr(usize)]
pub(super) enum HostTimer{Save=1,Desktop=2,Icons=5,Raise=9,RepairLayer=10,Foreground=11,OutsideSelection=12,Clipboard=13,ExternalDrop=14}
#[repr(usize)]
pub(super) enum FenceTimer{Folder=3,Peek=4,CancelInteraction=6,Autoscroll=7,Notice=8,Selection=9,MenuPrewarm=10}
#[repr(usize)]
pub(super) enum MenuTimer{Cancel=1,#[cfg(test)]Probe=2}

#[cfg(test)]mod tests{
    use super::super::*;
    fn distinct(scope:&str,values:&[(&str,usize)]){
        let mut assigned=std::collections::HashMap::new();
        for (name,value) in values{assert!(assigned.insert(*value,*name).is_none(),"{scope}: {name} reuses message/timer {value:#x}");}
    }
    #[test]
    fn desktop_message_ids_are_unique_across_all_receivers(){
        let values=[("tray",host::WM_TRAY), ("desktop changed",host::WM_DESKTOP_CHANGED), ("folder changed",host::WM_FOLDER_CHANGED),
            ("icon ready",host::WM_ICON_READY), ("double click",host::WM_DESKTOP_DBLCLICK), ("rename done",host::WM_RENAME_DONE),
            ("command request",WM_COMMAND_REQUEST), ("rename request",WM_RENAME_REQUEST), ("foreground",WM_FOREGROUND), ("repair layer",WM_REPAIR_LAYER),
            ("menu ready",menu_worker::READY), ("menu cancel",menu_worker::CANCEL_REQUEST), ("outside selection",outside_selection::MESSAGE),("external drop",external_drop::MESSAGE),("mouse input",mouse_input::MESSAGE)];
        distinct("desktop messages",&values.map(|(name,value)|(name,value as usize)));
        assert!(values.iter().all(|(_,value)|*value>=WM_APP&&*value<0xc000),"private messages overlap Windows registered messages");
        let registered=[("taskbar created",unsafe{RegisterWindowMessageW(w!("TaskbarCreated"))}),
            ("file operation result",zsdesk_windows::shell::file_transfer::result_message()),("menu failure",zsdesk_windows::shell::menu::failure_message())];
        assert!(registered.iter().all(|(_,value)|*value>=0xc000),"registered desktop protocol unavailable");
        distinct("registered messages",&registered.map(|(name,value)|(name,value as usize)));
    }
    #[test]
    fn desktop_timers_are_unique_within_each_receiving_window(){
        distinct("desktop host",&[("save",host::TIMER_SAVE),("desktop refresh",host::TIMER_DESKTOP),("icons",host::TIMER_ICONS),
            ("raise",TIMER_RAISE),("repair layer",TIMER_REPAIR_LAYER),("foreground",TIMER_FOREGROUND),("outside selection",outside_selection::TIMER),("clipboard",clipboard::TIMER),("external drop",external_drop::TIMER)]);
        distinct("fence",&[("folder refresh",host::TIMER_FOLDER),("peek",host::TIMER_PEEK),("cancel interaction",TIMER_CANCEL_INTERACTION),
            ("autoscroll",host::TIMER_AUTOSCROLL),("notice",host::TIMER_NOTICE),("selection",host::TIMER_SELECTION),("menu prewarm",super::FenceTimer::MenuPrewarm as usize)]);
        distinct("menu owner",&[("cancel",menu_worker::CANCEL_TIMER),("probe",menu_worker::PROBE_TIMER)]);
    }
}
