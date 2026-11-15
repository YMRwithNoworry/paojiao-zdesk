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

inline HRESULT NamespaceClass(IShellItem* item,CLSID& value){
    if(!item)return E_POINTER;
    ComPtr<IPersist> persist;HRESULT result=item->BindToHandler(nullptr,BHID_SFObject,IID_PPV_ARGS(&persist));
    if(SUCCEEDED(result))result=persist->GetClassID(&value);
    if(SUCCEEDED(result))return result;
    ComPtr<IShellItem2> properties;result=item->QueryInterface(IID_PPV_ARGS(&properties));
    static constexpr PROPERTYKEY namespaceClsid={{0x28636aa6,0x953d,0x11d2,{0xb5,0xd6,0x00,0xc0,0x4f,0xd9,0x18,0xd0}},6};
    return FAILED(result)?result:properties->GetCLSID(namespaceClsid,&value);
}
inline bool KnownNamespace(const CLSID& value,UINT& context){
    struct Entry{CLSID id;UINT context;};
    // Class/category pairs observed in this exact signed Shell build's
    // GetLocationContext, obtained through public namespace properties.
    static constexpr Entry entries[]={
        {{0x645ff040,0x5081,0x101b,{0x9f,0x08,0x00,0xaa,0x00,0x2f,0x95,0x4e}},0x20},
        {{0x20d04fe0,0x3aea,0x1069,{0xa2,0xd8,0x08,0x00,0x2b,0x30,0x30,0x9d}},0x40},
        {{0xf02c1a0d,0xbe21,0x4350,{0x88,0xb0,0x73,0x67,0xfc,0x96,0xef,0x3c}},0x40},
        {{0x031e4825,0x7b94,0x4dc3,{0xb1,0x31,0xe9,0x46,0xb4,0x4c,0x8d,0xd5}},0x40},
        {{0xa5a3563a,0x5755,0x4a6f,{0x85,0x4e,0xaf,0xa3,0x23,0x0b,0x19,0x9f}},0x80},
        {{0x26ee0668,0xa00a,0x44d7,{0x93,0x71,0xbe,0xb0,0x64,0xc9,0x86,0x83}},0x100},
        {{0xe7e4bc40,0xe76a,0x11ce,{0xa9,0xbb,0x00,0xaa,0x00,0x4a,0xe8,0x37}},0x200},
        {{0x2cc5ca98,0x6485,0x489a,{0x92,0x0e,0xb3,0xe8,0x8a,0x6c,0xcc,0xe3}},0x400},
    };
    for(const auto& entry:entries)if(value==entry.id){context=entry.context;return true;}return false;
}
inline HRESULT ContextKind(IFolderView2* view,UINT count,PCIDLIST_ABSOLUTE const* pidls,bool background,UINT& context){
    context=0;ComPtr<IShellItem> folder;HRESULT result=view->GetFolder(IID_PPV_ARGS(&folder));if(FAILED(result))return result;
    CLSID folderClass{};const bool hasFolderClass=SUCCEEDED(NamespaceClass(folder.Get(),folderClass));
    static constexpr CLSID desktop={0x00021400,0x0000,0x0000,{0xc0,0x00,0x00,0x00,0x00,0x00,0x00,0x46}};
    const bool isDesktop=hasFolderClass&&folderClass==desktop;
    if(background){
        SFGAOF attributes=0;UINT known=0;
        const bool ordinary=SUCCEEDED(folder->GetAttributes(SFGAO_FOLDER|SFGAO_FILESYSTEM|SFGAO_STREAM,&attributes))
            &&(attributes&(SFGAO_FOLDER|SFGAO_FILESYSTEM))==(SFGAO_FOLDER|SFGAO_FILESYSTEM)&&!(attributes&SFGAO_STREAM);
        if(!isDesktop&&!ordinary&&!(hasFolderClass&&KnownNamespace(folderClass,known)))return HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED);
        context=isDesktop?0x10001:2;return S_OK;
    }
    if(!count||count>8192||!pidls)return E_INVALIDARG;
    UINT category=0;
    for(UINT index=0;index<count;++index){
        ComPtr<IShellItem> item;result=SHCreateItemFromIDList(pidls[index],IID_PPV_ARGS(&item));if(FAILED(result))return result;
        SFGAOF attributes=0;UINT current=0;
        if(item->GetAttributes(SFGAO_FILESYSTEM,&attributes)==S_OK&&(attributes&SFGAO_FILESYSTEM))current=8;
        else{CLSID value{};if(FAILED(NamespaceClass(item.Get(),value))||!KnownNamespace(value,current))return HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED);}
        if(index&&current!=category)return HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED);category=current;
    }
    context=category|(isDesktop?0x10000:0);return S_OK;
}
inline HRESULT SelectionMatches(IFolderView2* view,UINT count,PCIDLIST_ABSOLUTE const* pidls){
    ComPtr<IShellItemArray> items;HRESULT result=view->GetSelection(FALSE,&items);if(FAILED(result))return result;
    DWORD actual=0;result=items->GetCount(&actual);if(FAILED(result))return result;if(actual!=count)return E_INVALIDARG;
    std::vector<ComPtr<IShellItem>> expected(count);std::vector<bool> used(count,false);
    for(UINT index=0;index<count;++index){result=SHCreateItemFromIDList(pidls[index],IID_PPV_ARGS(&expected[index]));if(FAILED(result))return result;}
    for(UINT index=0;index<count;++index){ComPtr<IShellItem> item;result=items->GetItemAt(index,&item);if(FAILED(result))return result;bool found=false;
        for(UINT candidate=0;candidate<count;++candidate){if(used[candidate])continue;int comparison=1;
            if(SUCCEEDED(item->Compare(expected[candidate].Get(),SICHINT_CANONICAL,&comparison))&&comparison==0){used[candidate]=true;found=true;break;}}
        if(!found)return E_INVALIDARG;
    }
    return S_OK;
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
    HRESULT WaitForView(HWND owner,IShellItem* expected){
        const ULONGLONG deadline=GetTickCount64()+3000;
        while(GetTickCount64()<deadline&&IsWindow(owner)){
            HRESULT result=browser_->GetCurrentView(IID_PPV_ARGS(&view_));
            if(SUCCEEDED(result)&&view_){
                result=view_.As(&folderView_);ComPtr<IShellItem> actual;int comparison=1;
                if(SUCCEEDED(result))result=folderView_->GetFolder(IID_PPV_ARGS(&actual));
                if(SUCCEEDED(result)&&SUCCEEDED(actual->Compare(expected,SICHINT_CANONICAL,&comparison))&&comparison==0)return S_OK;
            }
            quit_=PumpPending();if(quit_.quit)return E_ABORT;
            MsgWaitForMultipleObjectsEx(0,nullptr,10,QS_ALLINPUT,MWMO_INPUTAVAILABLE);
        }
        return HRESULT_FROM_WIN32(ERROR_TIMEOUT);
    }
    HRESULT Track(HWND owner,IContextMenu* context,HMENU menu,UINT count,
            PCIDLIST_ABSOLUTE const* pidls,IShellItem* folder,bool background,IExplorerBrowser* sourceBrowser,UINT inputFlags,POINT* point,
            const UINT* applications,UINT applicationCount,uint32_t intercept,
            menu_dispatch::BeforeInvoke before,void* beforeContext,menu_dispatch::Result* outcome){
        if(!IsWindow(owner)||GetWindowThreadProcessId(owner,nullptr)!=GetCurrentThreadId()||
                !context||!menu||!point||!outcome||(!background&&(!count||!pidls))||(background&&!folder)||
                (applicationCount&&!applications)||!before||(inputFlags&~3u)||((inputFlags&2u)&&!(inputFlags&1u)))return E_INVALIDARG;
        *outcome={};
        HRESULT result=S_OK;
        // The source view owns both the folder services and the immutable
        // request selection before native Prepare examines either interface.
        ComPtr<IShellItem> selectedTarget;
        if(sourceBrowser){
            // Keep background verbs and presenter services on the same view.
            // The worker owns this browser and applies its layout result later.
            reused_=true;browser_=sourceBrowser;
            result=browser_->GetCurrentView(IID_PPV_ARGS(&view_));
            if(SUCCEEDED(result))result=view_.As(&folderView_);
        }else if(background){
            container_=CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,L"STATIC",L"",WS_POPUP,0,0,1,1,nullptr,nullptr,GetModuleHandleW(nullptr),nullptr);
            if(!container_)return HRESULT_FROM_WIN32(GetLastError());
            result=CoCreateInstance(CLSID_ExplorerBrowser,nullptr,CLSCTX_INPROC_SERVER,IID_PPV_ARGS(&browser_));
            RECT bounds{0,0,1,1};
            if(SUCCEEDED(result))result=browser_->SetOptions(EBO_NOBORDER|EBO_NAVIGATEONCE);
            if(SUCCEEDED(result))result=browser_->Initialize(container_,&bounds,nullptr);
            if(SUCCEEDED(result))result=browser_->BrowseToObject(folder,0);
            if(SUCCEEDED(result))result=WaitForView(owner,folder);
        }else return E_NOTIMPL;
        if(FAILED(result))return result;
        if(!background){
            for(UINT index=0;index<count;++index)if(!pidls[index])return E_INVALIDARG;
            result=SelectionMatches(folderView_.Get(),count,pidls);if(FAILED(result))return result;
            result=SHCreateItemFromIDList(pidls[0],IID_PPV_ARGS(&selectedTarget));if(FAILED(result))return result;
        }else{ComPtr<IShellItem> actual;int comparison=1;result=folderView_->GetFolder(IID_PPV_ARGS(&actual));
            if(FAILED(result)||FAILED(actual->Compare(folder,SICHINT_CANONICAL,&comparison))||comparison!=0)return E_INVALIDARG;}
        UINT contextKind=0;result=ContextKind(folderView_.Get(),count,pidls,background,contextKind);if(FAILED(result))return result;
        extra_.value=CreatePopupMenu();if(!extra_.value)return E_OUTOFMEMORY;
        result=CopyApplicationItems(menu,extra_.value);if(FAILED(result))return result;
        result=QueryMenuPresenter(browser_.Get(),kPresenterSID,kPresenterIID,reinterpret_cast<void**>(presenter_.GetAddressOf()),owner);
        if(FAILED(result)||!active)return FAILED(result)?result:E_UNEXPECTED;
        session_=active;session_->AddRef();
        result=session_->ConfigureStandalone(owner,GetMenuItemCount(extra_.value)>0?extra_.value:nullptr,*point,
            applications,applicationCount,intercept,before,beforeContext,context,background?0:count,pidls);
        if(FAILED(result))return result;
        if(SUCCEEDED(context->QueryInterface(IID_PPV_ARGS(&site_)))){
            site_->GetSite(IID_PPV_ARGS(&previousSite_));
            result=site_->SetSite(session_->MenuSite());
            if(FAILED(result))return result;restoreSite_=true;
        }else return E_NOINTERFACE;
        result=presenter_->PrepareForContextMenu(contextKind,session_->MenuSite(),*point,context,1,0x6fff,TRUE,nullptr,GUID{},background?folder:selectedTarget.Get());
        if(FAILED(result))return result;
        if(session_->Cancelled())return E_ABORT;
        if(!background){result=SelectionMatches(folderView_.Get(),count,pidls);if(FAILED(result))return result;++gSelectionSnapshots;}
        result=session_->CommandPreparationFailure();if(FAILED(result))return result;
        // The native projection snapshots HMENU children. Initialize empty
        // legacy submenus here, where failure can abort before presentation.
        const auto commandGeneration=session_->CommandGeneration();
        result=session_->InitializeSubmenus(menu);if(FAILED(result))return result;
        if(session_->Cancelled()||!IsWindow(owner)||!IsMenu(menu))return E_ABORT;
        if(!background){result=SelectionMatches(folderView_.Get(),count,pidls);if(FAILED(result))return result;}
        if(!session_->CommandsCurrent(commandGeneration))return E_ABORT;
        const auto failures=menu_model_filter::failures.load();
        if(session_->Cancelled())return E_ABORT;
        session_->ArmCommands();
        outcome->phase=menu_dispatch::Presented;
        menu_model_filter::ProjectionProbe projection(menu,GetEnvironmentVariableW(L"ZSDESK_MENU_TEST_PROBE",nullptr,0)!=0);
        presenter_->DoContextMenu(menu,(inputFlags&1u)?1:9,GUID{});
        if(menu_model_filter::failures.load()!=failures)return E_UNEXPECTED;
        // Keep Shell callbacks and XAML command objects alive until the native
        // completion callback. IsContextMenuOpen is already false while a click
        // is still queued behind the closing animation.
        bool aborted=false;
        for(;;){
            if(!quit_.quit&&IsWindow(owner)&&session_->CallbackReady()){
                // This executes after the native dismiss callback has unwound,
                // but before waiting for its deferred legacy command result.
                if(restoreSite_&&site_){result=site_->SetSite(previousSite_.Get());if(FAILED(result))return result;restoreSite_=false;}
                session_->DispatchPending();
            }
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
        // The presenter site is only for native preparation. Shell property
        // sheets and legacy extensions require their original view services
        // during InvokeCommand, including any asynchronous UI they create.
        if(restoreSite_&&site_){
            result=site_->SetSite(previousSite_.Get());
            if(FAILED(result))return result;restoreSite_=false;
        }
        if(!aborted)session_->DispatchPending();
        *outcome=aborted?menu_dispatch::Result{}:session_->StandaloneResult();
        if(GetEnvironmentVariableW(L"ZSDESK_MENU_TEST_PROBE",nullptr,0))std::fprintf(stderr,"native-menu-projection input_tree_observed=%d lazy_initialized=%zu\n",projection.Observed(),session_->InitializedMenuCount());
        if(gProbeExpanded)std::fprintf(stderr,"expand track: aborted=%d quit=%d cancelled=%d kind=%u status=0x%08X\n",aborted,quit_.quit,session_->Cancelled(),outcome->kind,static_cast<unsigned>(outcome->status));
        outcome->phase=(std::max)(outcome->phase,static_cast<uint32_t>(menu_dispatch::Presented));
        if(outcome->kind==menu_dispatch::Expanded)*point=session_->StandalonePoint();
        return S_OK;
    }
};
} // namespace standalone_menu

extern "C" HRESULT zs_menu_track_standalone(HWND owner,IContextMenu* context,HMENU menu,
        UINT count,PCIDLIST_ABSOLUTE const* pidls,IShellItem* folder,BOOL background,IExplorerBrowser* sourceBrowser,UINT inputFlags,POINT* point,
        const UINT* applications,UINT applicationCount,uint32_t intercept,
        menu_dispatch::BeforeInvoke before,void* beforeContext,menu_dispatch::Result* outcome) noexcept {
    if(!outcome)return E_POINTER;*outcome={};
    try{
        standalone_menu::Host host;
        const HRESULT result=host.Track(owner,context,menu,count,pidls,folder,background!=FALSE,sourceBrowser,inputFlags,point,
            applications,applicationCount,intercept,before,beforeContext,outcome);
        if(FAILED(result))InterlockedExchange(&gLastFailure,result);
        return result;
    }catch(const std::bad_alloc&){InterlockedExchange(&gLastFailure,E_OUTOFMEMORY);return E_OUTOFMEMORY;}
    catch(...){InterlockedExchange(&gLastFailure,E_UNEXPECTED);return E_UNEXPECTED;}
}
