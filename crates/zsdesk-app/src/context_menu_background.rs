//! A real namespace view owns virtual-folder background verbs in the worker.
use super::*;
use windows::{Win32::{Foundation::RECT,System::Com::{CoCreateInstance,CLSCTX_INPROC_SERVER},UI::Shell::{
    IExplorerBrowser,IShellView,IShellItem,ExplorerBrowser,EBO_NOBORDER,EBO_NOPERSISTVIEWSTATE}}};

pub(super) struct BackgroundView { pub browser:IExplorerBrowser,pub view:IShellView,window:HWND }
impl BackgroundView {
    pub fn create(item:&IShellItem,request:&Request,events:&EventSet)->Result<Self> {
        let window=unsafe{CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,w!("STATIC"),w!(""),WS_POPUP,0,0,640,480,None,None,None,None)}?;
        let result=(||->Result<(IExplorerBrowser,IShellView)>{
            let browser:IExplorerBrowser=unsafe{CoCreateInstance(&ExplorerBrowser,None,CLSCTX_INPROC_SERVER)}?;
            let result=(||->Result<IShellView>{
                unsafe{browser.SetOptions(EBO_NOBORDER|EBO_NOPERSISTVIEWSTATE)?;
                    browser.Initialize(window,&RECT{left:0,top:0,right:640,bottom:480},None)?;browser.BrowseToObject(item,0)?;}
                let deadline=Instant::now()+Duration::from_secs(5);
                loop {
                    if let Ok(view)=unsafe{browser.GetCurrentView::<IShellView>()}{return Ok(view);}
                    ensure!(owner_alive(request)&&!EventSet::signaled(events.cancel),"背景菜单已取消");
                    ensure!(Instant::now()<deadline,"背景菜单视图加载超时");pump(10);
                }
            })();
            match result{Ok(view)=>Ok((browser,view)),Err(error)=>{let _=unsafe{browser.Destroy()};Err(error)}}
        })();
        match result{Ok((browser,view))=>Ok(Self{browser,view,window}),Err(error)=>{let _=unsafe{DestroyWindow(window)};Err(error)}}
    }
}
impl Drop for BackgroundView {fn drop(&mut self){let _=unsafe{self.browser.Destroy()};let _=unsafe{DestroyWindow(self.window)};}}
