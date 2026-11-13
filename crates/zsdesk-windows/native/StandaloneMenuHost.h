#pragma once
#include <climits>
#include <wrl/client.h>

namespace standalone_menu {
using Microsoft::WRL::ComPtr;
struct Menu {
    HMENU value=nullptr;
    ~Menu(){if(value)DestroyMenu(value);}
};
inline HRESULT CopyApplicationItems(HMENU source,HMENU destination) {
    bool separator=true;
    const int count=GetMenuItemCount(source);
    if(count<0)return E_INVALIDARG;
    for(int index=0;index<count;++index){
        MENUITEMINFOW item{};item.cbSize=sizeof(item);
        item.fMask=MIIM_ID|MIIM_FTYPE|MIIM_STATE|MIIM_SUBMENU|MIIM_DATA|MIIM_BITMAP;
        if(!GetMenuItemInfoW(source,index,TRUE,&item))return HRESULT_FROM_WIN32(GetLastError());
        if(item.fType&MFT_SEPARATOR){
            if(!separator){if(!AppendMenuW(destination,MF_SEPARATOR,0,nullptr))return E_FAIL;separator=true;}
            continue;
        }
        Menu submenu;
        if(item.hSubMenu){
            submenu.value=CreatePopupMenu();if(!submenu.value)return E_OUTOFMEMORY;
            HRESULT result=CopyApplicationItems(item.hSubMenu,submenu.value);if(FAILED(result))return result;
            if(GetMenuItemCount(submenu.value)==0)continue;
            item.hSubMenu=submenu.value;
        }else{
            if(item.wID<0x7000||item.wID==UINT_MAX)continue;
            if(item.wID>=0x7c00)return E_INVALIDARG;
        }
        MENUITEMINFOW text{};text.cbSize=sizeof(text);text.fMask=MIIM_STRING;
        if(!GetMenuItemInfoW(source,index,TRUE,&text))return E_FAIL;
        std::vector<wchar_t> label(static_cast<size_t>(text.cch)+1);
        text.dwTypeData=label.data();text.cch=static_cast<UINT>(label.size());
        if(!GetMenuItemInfoW(source,index,TRUE,&text))return E_FAIL;
        item.fMask|=MIIM_STRING;item.dwTypeData=label.data();item.cch=text.cch;
        if(!InsertMenuItemW(destination,GetMenuItemCount(destination),TRUE,&item))return E_FAIL;
        submenu.value=nullptr;separator=false;
    }
    // A final separator separates application functions from native commands.
    if(!separator&&GetMenuItemCount(destination)>0&&!AppendMenuW(destination,MF_SEPARATOR,0,nullptr))return E_FAIL;
    return S_OK;
}

struct PumpResult { bool quit=false;int exitCode=0; };
inline PumpResult PumpPending() {
    MSG message{};PumpResult result;
    while(PeekMessageW(&message,nullptr,0,0,PM_REMOVE)){
        if(message.message==WM_QUIT){result.quit=true;result.exitCode=static_cast<int>(message.wParam);break;}
        TranslateMessage(&message);DispatchMessageW(&message);
    }
    return result;
}

struct CachedResults {
    HWND container=nullptr;
    ComPtr<IExplorerBrowser> browser;
    ComPtr<IShellView> view;
    ComPtr<IFolderView2> folderView;
    HRESULT status=E_PENDING;
};
// One lazy Shell view per UI thread keeps DirectUI initialized. Destroying the
// last CItemsView from an input/menu callback can deadlock TSF's monitor thread.
// The independent hidden window is never parented to a fence. Its empty view
// lives until process exit, like the shared XAML runtime owner; no user data or
// per-menu presenter is retained here.
inline thread_local CachedResults* cachedResults=nullptr;

inline HRESULT EnsureResultsHost(HWND owner,PumpResult& quit){
    if(cachedResults)return cachedResults->status;
    cachedResults=new(std::nothrow) CachedResults;if(!cachedResults)return E_OUTOFMEMORY;
    auto& cache=*cachedResults;
    cache.container=CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,L"STATIC",L"",WS_POPUP,0,0,1,1,nullptr,nullptr,GetModuleHandleW(nullptr),nullptr);
    HRESULT result=cache.container?S_OK:HRESULT_FROM_WIN32(GetLastError());
    if(SUCCEEDED(result))result=CoCreateInstance(CLSID_ExplorerBrowser,nullptr,CLSCTX_INPROC_SERVER,IID_PPV_ARGS(&cache.browser));
    RECT bounds{0,0,1,1};
    if(SUCCEEDED(result))result=cache.browser->SetOptions(EBO_NOBORDER|EBO_NAVIGATEONCE);
    if(SUCCEEDED(result))result=cache.browser->Initialize(cache.container,&bounds,nullptr);
    if(SUCCEEDED(result))result=cache.browser->FillFromObject(nullptr,EBF_NODROPTARGET);
    if(SUCCEEDED(result)){
        const auto deadline=GetTickCount64()+3000;
        do{
            result=cache.browser->GetCurrentView(IID_PPV_ARGS(&cache.view));
            if(SUCCEEDED(result)&&cache.view){result=cache.view.As(&cache.folderView);break;}
            quit=PumpPending();if(quit.quit||!IsWindow(owner)){result=E_ABORT;break;}
            MsgWaitForMultipleObjectsEx(0,nullptr,10,QS_ALLINPUT,MWMO_INPUTAVAILABLE);
        }while(GetTickCount64()<deadline);
    }
    cache.status=result;return result;
}

class Host {
    HWND container_=nullptr;
    ComPtr<IExplorerBrowser> browser_;
    ComPtr<IShellView> view_;
    ComPtr<IFolderView2> folderView_;
    ComPtr<Presenter> presenter_;
    ComPtr<IObjectWithSite> site_;
    ComPtr<IUnknown> previousSite_;
    Session* session_=nullptr;
    Menu extra_;
    bool restoreSite_=false;
    bool reused_=false;
    PumpResult quit_;
public:
    ~Host(){
        if(restoreSite_&&site_)site_->SetSite(previousSite_.Get());
        if(session_){
            if(active==session_)zs_menu_shutdown();else session_->Retire();
            session_->Release();
        }
        presenter_.Reset();site_.Reset();previousSite_.Reset();folderView_.Reset();view_.Reset();
        if(browser_){
            if(!reused_)browser_->Destroy();
            browser_.Reset();
        }
        if(container_&&IsWindow(container_))DestroyWindow(container_);
        if(quit_.quit)PostQuitMessage(quit_.exitCode);
    }
    HRESULT WaitForView(HWND owner){
        const ULONGLONG deadline=GetTickCount64()+3000;
        while(GetTickCount64()<deadline&&IsWindow(owner)){
            HRESULT result=browser_->GetCurrentView(IID_PPV_ARGS(&view_));
            if(SUCCEEDED(result)&&view_)return view_.As(&folderView_);
            quit_=PumpPending();if(quit_.quit)return E_ABORT;
            MsgWaitForMultipleObjectsEx(0,nullptr,10,QS_ALLINPUT,MWMO_INPUTAVAILABLE);
        }
        return HRESULT_FROM_WIN32(ERROR_TIMEOUT);
    }
    HRESULT Track(HWND owner,IContextMenu* context,HMENU menu,UINT count,
            PCIDLIST_ABSOLUTE const* pidls,IShellItem* folder,bool background,IExplorerBrowser* sourceBrowser,POINT* point,
            const UINT* applications,UINT applicationCount,uint32_t intercept,
            menu_dispatch::BeforeInvoke before,void* beforeContext,menu_dispatch::Result* outcome){
        if(!IsWindow(owner)||GetWindowThreadProcessId(owner,nullptr)!=GetCurrentThreadId()||
                !context||!menu||!point||!outcome||(!background&&(!count||!pidls))||(background&&!folder)||
                (applicationCount&&!applications))return E_INVALIDARG;
        *outcome={};
        HRESULT result=EnsureResultsHost(owner,quit_);if(FAILED(result))return result;
        // The private view supplies Shell services only. Selection is passed as
        // an immutable absolute-PIDL snapshot, never as another view's state.
        ComPtr<IShellItem> selectedTarget;
        if(!background){
            reused_=true;browser_=cachedResults->browser;view_=cachedResults->view;folderView_=cachedResults->folderView;
            for(UINT index=0;index<count;++index)if(!pidls[index])return E_INVALIDARG;
            result=SHCreateItemFromIDList(pidls[0],IID_PPV_ARGS(&selectedTarget));
        }else if(sourceBrowser){
            // Keep background verbs and presenter services on the same view.
            // The worker owns this browser and applies its layout result later.
            reused_=true;browser_=sourceBrowser;
            result=browser_->GetCurrentView(IID_PPV_ARGS(&view_));
            if(SUCCEEDED(result))result=view_.As(&folderView_);
        }else{
            container_=CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,L"STATIC",L"",WS_POPUP,0,0,1,1,nullptr,nullptr,GetModuleHandleW(nullptr),nullptr);
            if(!container_)return HRESULT_FROM_WIN32(GetLastError());
            result=CoCreateInstance(CLSID_ExplorerBrowser,nullptr,CLSCTX_INPROC_SERVER,IID_PPV_ARGS(&browser_));
            RECT bounds{0,0,1,1};
            if(SUCCEEDED(result))result=browser_->SetOptions(EBO_NOBORDER|EBO_NAVIGATEONCE);
            if(SUCCEEDED(result))result=browser_->Initialize(container_,&bounds,nullptr);
            if(SUCCEEDED(result))result=browser_->BrowseToObject(folder,0);
            if(SUCCEEDED(result))result=WaitForView(owner);
        }
        if(FAILED(result))return result;
        extra_.value=CreatePopupMenu();if(!extra_.value)return E_OUTOFMEMORY;
        result=CopyApplicationItems(menu,extra_.value);if(FAILED(result))return result;
        result=QueryMenuPresenter(browser_.Get(),kPresenterSID,kPresenterIID,reinterpret_cast<void**>(presenter_.GetAddressOf()),owner);
        if(FAILED(result)||!active)return FAILED(result)?result:E_UNEXPECTED;
        session_=active;session_->AddRef();
        session_->ConfigureStandalone(owner,GetMenuItemCount(extra_.value)>0?extra_.value:nullptr,*point,
            applications,applicationCount,intercept,before,beforeContext);
        if(SUCCEEDED(context->QueryInterface(IID_PPV_ARGS(&site_)))){
            site_->GetSite(IID_PPV_ARGS(&previousSite_));
            result=site_->SetSite(static_cast<IServiceProvider*>(session_));
            if(FAILED(result))return result;restoreSite_=true;
        }else return E_NOINTERFACE;
        result=presenter_->PrepareForContextMenu(background?12:8,static_cast<IServiceProvider*>(session_),*point,context,1,0x7bff,TRUE,nullptr,GUID{},nullptr);
        if(FAILED(result))return result;
        if(session_->Cancelled())return E_ABORT;
        if(!background){
            // Preserve the original absolute identifiers, independently of the
            // temporary results namespace and its view lifetime.
            ComPtr<IShellItemArray> source,snapshot;
            result=SHCreateShellItemArrayFromIDLists(count,const_cast<PCIDLIST_ABSOLUTE*>(pidls),&source);
            if(SUCCEEDED(result))result=menu_selection_agile::Selection::Create(source.Get(),&snapshot);
            if(FAILED(result))return result;
            auto slot=reinterpret_cast<IShellItemArray**>(reinterpret_cast<BYTE*>(presenter_.Get())+0x60);
            IShellItemArray* previous=*slot;*slot=snapshot.Detach();if(previous)previous->Release();
            ++gSelectionSnapshots;
        }
        session_->ConfigureStandalone(owner,GetMenuItemCount(extra_.value)>0?extra_.value:nullptr,*point,
            applications,applicationCount,intercept,before,beforeContext);
        const auto failures=menu_model_filter::failures.load();
        if(session_->Cancelled())return E_ABORT;
        outcome->phase=menu_dispatch::Presented;
        presenter_->DoContextMenu(menu,9,GUID{});
        if(menu_model_filter::failures.load()!=failures)return E_UNEXPECTED;
        // Keep Shell callbacks and XAML command objects alive until the native
        // completion callback. IsContextMenuOpen is already false while a click
        // is still queued behind the closing animation.
        bool aborted=false;
        for(;;){
            const auto wait=session_->StandaloneWait(IsWindow(owner)!=FALSE,quit_.quit,presenter_->IsContextMenuOpen()!=FALSE,GetTickCount64());
            if(wait==menu_dispatch::Completion::Complete)break;
            if(wait==menu_dispatch::Completion::Aborted){aborted=true;break;}
            if(wait==menu_dispatch::Completion::TimedOut)return HRESULT_FROM_WIN32(ERROR_TIMEOUT);
            MSG message{};
            if(PeekMessageW(&message,nullptr,0,0,PM_REMOVE)){
                if(message.message==WM_QUIT){quit_.quit=true;quit_.exitCode=static_cast<int>(message.wParam);continue;}
                TranslateMessage(&message);DispatchMessageW(&message);
            }
            MsgWaitForMultipleObjectsEx(0,nullptr,20,QS_ALLINPUT,MWMO_INPUTAVAILABLE);
        }
        *outcome=aborted?menu_dispatch::Result{}:session_->StandaloneResult();
        outcome->phase=(std::max)(outcome->phase,static_cast<uint32_t>(menu_dispatch::Presented));
        if(outcome->kind==menu_dispatch::Expanded)*point=session_->StandalonePoint();
        return S_OK;
    }
};
} // namespace standalone_menu

extern "C" HRESULT zs_menu_track_standalone(HWND owner,IContextMenu* context,HMENU menu,
        UINT count,PCIDLIST_ABSOLUTE const* pidls,IShellItem* folder,BOOL background,IExplorerBrowser* sourceBrowser,POINT* point,
        const UINT* applications,UINT applicationCount,uint32_t intercept,
        menu_dispatch::BeforeInvoke before,void* beforeContext,menu_dispatch::Result* outcome) noexcept {
    if(!outcome)return E_POINTER;*outcome={};
    try{
        standalone_menu::Host host;
        const HRESULT result=host.Track(owner,context,menu,count,pidls,folder,background!=FALSE,sourceBrowser,point,
            applications,applicationCount,intercept,before,beforeContext,outcome);
        if(FAILED(result))InterlockedExchange(&gLastFailure,result);
        return result;
    }catch(const std::bad_alloc&){InterlockedExchange(&gLastFailure,E_OUTOFMEMORY);return E_OUTOFMEMORY;}
    catch(...){InterlockedExchange(&gLastFailure,E_UNEXPECTED);return E_UNEXPECTED;}
}
