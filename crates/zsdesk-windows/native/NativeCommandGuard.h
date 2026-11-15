#pragma once
#include <windows.h>
#include <shlobj.h>
#include <shobjidl.h>
#include <servprov.h>
#include <docobj.h>
#include <roapi.h>
#include <wrl/client.h>
#include <atomic>
#include <memory>
#include <mutex>
#include <unordered_map>
#include <vector>
#include <algorithm>
#include <cstdio>
#include "MenuCommandDispatch.h"

// Request-scoped authorization for native IExplorerCommand objects. Proxies
// retain only a weak gate. The gate's callback is used solely on its owner STA
// and is invalidated before the caller's stack/context can be released.
namespace native_command_guard {
using Microsoft::WRL::ComPtr;
using ResolveAction=HRESULT(WINAPI*)(void*,UINT,menu_dispatch::Result*,BOOL*);
inline std::atomic<ULONGLONG> nextToken{1};
inline std::atomic<uint64_t> authorized{0},denied{0},invoked{0},wrapped{0},unsupported{0},unobserved{0};
inline std::mutex unsupportedMutex;
inline std::vector<GUID> unsupportedInterfaces;
inline void RecordUnsupported(REFIID iid)noexcept{
    ++unsupported;
    try{
        std::lock_guard<std::mutex> lock(unsupportedMutex);
        if(unsupportedInterfaces.size()>=16||std::find(unsupportedInterfaces.begin(),unsupportedInterfaces.end(),iid)!=unsupportedInterfaces.end())return;
        unsupportedInterfaces.push_back(iid);
        std::fprintf(stderr,"native-command-proxy unimplemented interface iid=%08lX-%04X-%04X-%02X%02X-%02X%02X%02X%02X%02X%02X; QI=E_NOINTERFACE\n",iid.Data1,iid.Data2,iid.Data3,iid.Data4[0],iid.Data4[1],iid.Data4[2],iid.Data4[3],iid.Data4[4],iid.Data4[5],iid.Data4[6],iid.Data4[7]);
    }catch(...){}
}
inline constexpr wchar_t kClass[]=L"ZSDesk.NativeCommandGate.v1";
inline constexpr wchar_t kProperty[]=L"ZSDesk.NativeCommandGate.Token.v1";
inline constexpr UINT kAuthorize=WM_APP+101,kDispose=WM_APP+102;
inline constexpr HRESULT kCancelled=HRESULT_FROM_WIN32(ERROR_CANCELLED);

struct Decision {HRESULT status=kCancelled;bool execute=false,handled=false;menu_dispatch::Result outcome{};};
enum class Phase {Fresh,Authorizing,Granted,Executing,Completed,Denied};
struct Snapshot {
    Phase phase=Phase::Fresh;UINT command=0;HRESULT failure=S_OK;
    menu_dispatch::Result outcome{};
    bool action()const noexcept{return phase==Phase::Executing||phase==Phase::Completed;}
    bool waiting()const noexcept{return phase==Phase::Authorizing||phase==Phase::Executing;}
};
class Gate;
struct WindowHolder {std::weak_ptr<Gate> gate;ULONGLONG token=0;};
struct Pending {
    HANDLE event=nullptr;ULONGLONG generation=0;UINT command=0;bool begin=false,resolve=false,checkSelection=false;
    ComPtr<IAgileReference> selection;Decision decision;std::atomic<bool> cancelled{false};
    Pending(){event=CreateEventW(nullptr,TRUE,FALSE,nullptr);}
    ~Pending(){if(event)CloseHandle(event);}
};

class Gate final:public std::enable_shared_from_this<Gate>{
    mutable std::mutex mutex_;
    HWND owner_=nullptr,dispatcher_=nullptr;DWORD ownerThread_=0;
    const ULONGLONG token_=nextToken.fetch_add(1);
    ULONGLONG generation_=0,nextRequest_=1;
    UINT selectedCommand_=0;
    bool alive_=true,armed_=false,requiresCommandIdentity_=false;
    menu_dispatch::BeforeInvoke before_=nullptr;void* beforeContext_=nullptr;
    ResolveAction resolve_=nullptr;void* resolveContext_=nullptr;
    std::vector<std::vector<BYTE>> expected_;
    Snapshot result_;
    std::unordered_map<ULONGLONG,std::shared_ptr<Pending>> pending_;

    bool WindowMatches()const noexcept{
        DWORD process=0;
        return IsWindow(owner_)&&GetWindowThreadProcessId(owner_,&process)==ownerThread_&&process==GetCurrentProcessId()
            &&reinterpret_cast<ULONG_PTR>(GetPropW(owner_,kProperty))==token_;
    }
    HRESULT SelectionMatches(IAgileReference* reference)const{
        if(expected_.empty())return S_OK; // Background providers may pass their folder item.
        if(!reference)return E_INVALIDARG;
        ComPtr<IShellItemArray> items;HRESULT hr=reference->Resolve(IID_PPV_ARGS(&items));if(FAILED(hr))return hr;
        DWORD count=0;hr=items->GetCount(&count);if(FAILED(hr))return hr;if(count!=expected_.size())return kCancelled;
        std::vector<std::vector<BYTE>> actual;actual.reserve(count);
        for(DWORD index=0;index<count;++index){
            ComPtr<IShellItem> item;hr=items->GetItemAt(index,&item);if(FAILED(hr))return hr;
            PIDLIST_ABSOLUTE id=nullptr;hr=SHGetIDListFromObject(item.Get(),&id);if(FAILED(hr))return hr;
            const UINT bytes=id?ILGetSize(id):0;if(bytes<2||bytes>65536){if(id)CoTaskMemFree(id);return E_INVALIDARG;}
            actual.emplace_back(reinterpret_cast<BYTE*>(id),reinterpret_cast<BYTE*>(id)+bytes);CoTaskMemFree(id);
        }
        auto expected=expected_;std::sort(expected.begin(),expected.end());std::sort(actual.begin(),actual.end());if(actual==expected)return S_OK;
        // Dates and attributes may change PIDL bytes without changing identity.
        std::vector<bool> used(expected_.size(),false);
        for(const auto& value:actual){
            ComPtr<IShellItem> item;hr=SHCreateItemFromIDList(reinterpret_cast<PCIDLIST_ABSOLUTE>(value.data()),IID_PPV_ARGS(&item));if(FAILED(hr))return hr;
            bool found=false;
            for(size_t index=0;index<expected_.size();++index){if(used[index])continue;
                ComPtr<IShellItem> candidate;hr=SHCreateItemFromIDList(reinterpret_cast<PCIDLIST_ABSOLUTE>(expected_[index].data()),IID_PPV_ARGS(&candidate));if(FAILED(hr))return hr;
                int compare=1;if(SUCCEEDED(item->Compare(candidate.Get(),SICHINT_CANONICAL,&compare))&&compare==0){used[index]=true;found=true;break;}
            }
            if(!found)return kCancelled;
        }
        return S_OK;
    }
    Decision OnOwner(const std::shared_ptr<Pending>& request){
        Decision decision;menu_dispatch::BeforeInvoke before=nullptr;void* beforeContext=nullptr;ResolveAction resolve=nullptr;void* resolveContext=nullptr;
        bool wasGranted=false;UINT command=request->command;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            if(!alive_||!armed_||request->generation!=generation_||request->cancelled.load()||!WindowMatches()){++denied;return decision;}
            if(selectedCommand_&&command!=selectedCommand_){++denied;return decision;}
            if(result_.phase==Phase::Fresh&&!command&&requiresCommandIdentity_){result_.failure=E_NOTIMPL;decision.status=E_NOTIMPL;++unsupported;return decision;}
            if(result_.phase==Phase::Granted){
                if(command&&result_.command&&command!=result_.command){++denied;return decision;}
                command=result_.command;wasGranted=true;
            }else if(result_.phase!=Phase::Fresh){++denied;return decision;}
            if(!wasGranted){result_.phase=Phase::Authorizing;result_.command=command;}
            before=before_;beforeContext=beforeContext_;resolve=resolve_;resolveContext=resolveContext_;
        }
        HRESULT hr=S_OK;menu_dispatch::Result action{};BOOL handled=FALSE;
        if(request->checkSelection)hr=SelectionMatches(request->selection.Get());
        {
            // Shell identity calls can pump messages and retire this request.
            std::lock_guard<std::mutex> lock(mutex_);
            if(!alive_||request->generation!=generation_||request->cancelled.load()||!WindowMatches()){++denied;return decision;}
            before=before_;beforeContext=beforeContext_;resolve=resolve_;resolveContext=resolveContext_;
        }
        if(SUCCEEDED(hr)&&!wasGranted&&request->resolve&&resolve)hr=resolve(resolveContext,command,&action,&handled);
        {
            std::lock_guard<std::mutex> lock(mutex_);
            if(!alive_||request->generation!=generation_||request->cancelled.load()||!WindowMatches()){++denied;return decision;}
            before=before_;beforeContext=beforeContext_;
        }
        if(SUCCEEDED(hr)&&!wasGranted&&!handled)hr=before?before(beforeContext):E_ACCESSDENIED;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            if(!alive_||request->generation!=generation_||request->cancelled.load()||!WindowMatches()){++denied;return decision;}
            if(FAILED(hr)){result_.phase=Phase::Denied;result_.outcome={menu_dispatch::Invoked,command,hr,menu_dispatch::Presented};decision.status=hr;++denied;return decision;}
            if(handled){result_.phase=Phase::Completed;result_.outcome=action;decision={S_OK,false,true,action};return decision;}
            if(!wasGranted)++authorized;
            result_.phase=request->begin?Phase::Executing:Phase::Granted;
            result_.command=command;result_.outcome={menu_dispatch::NativeHandled,command,E_PENDING,menu_dispatch::Executing};
            decision={S_OK,request->begin,false,result_.outcome};
        }
        return decision;
    }
    Decision SafeOnOwner(const std::shared_ptr<Pending>& request){
        HRESULT status=E_UNEXPECTED;
        try{return OnOwner(request);}catch(const std::bad_alloc&){status=E_OUTOFMEMORY;}catch(...){status=E_UNEXPECTED;}
        std::lock_guard<std::mutex> lock(mutex_);
        if(request->generation==generation_&&result_.phase==Phase::Authorizing){result_.phase=Phase::Denied;result_.outcome={menu_dispatch::Invoked,request->command,status,menu_dispatch::Presented};}
        ++denied;Decision result;result.status=status;return result;
    }
    static LRESULT CALLBACK Procedure(HWND window,UINT message,WPARAM wp,LPARAM lp){
        if(message==WM_NCCREATE){auto create=reinterpret_cast<CREATESTRUCTW*>(lp);SetWindowLongPtrW(window,GWLP_USERDATA,reinterpret_cast<LONG_PTR>(create->lpCreateParams));return TRUE;}
        auto holder=reinterpret_cast<WindowHolder*>(GetWindowLongPtrW(window,GWLP_USERDATA));
        if(message==WM_NCDESTROY){SetWindowLongPtrW(window,GWLP_USERDATA,0);delete holder;return DefWindowProcW(window,message,wp,lp);}
        if(holder&&static_cast<ULONGLONG>(wp)==holder->token){
            if(message==kDispose){DestroyWindow(window);return 0;}
            if(message==kAuthorize){if(auto gate=holder->gate.lock()){
                std::shared_ptr<Pending> request;
                {std::lock_guard<std::mutex> lock(gate->mutex_);auto found=gate->pending_.find(static_cast<ULONGLONG>(lp));if(found!=gate->pending_.end())request=found->second;}
                if(request){request->decision=gate->SafeOnOwner(request);SetEvent(request->event);
                    std::lock_guard<std::mutex> lock(gate->mutex_);gate->pending_.erase(static_cast<ULONGLONG>(lp));}
            }return 0;}
        }
        return DefWindowProcW(window,message,wp,lp);
    }
public:
    static HRESULT Create(HWND owner,menu_dispatch::BeforeInvoke before,void* beforeContext,ResolveAction resolve,void* resolveContext,
            UINT count,PCIDLIST_ABSOLUTE const* pidls,bool requiresCommandIdentity,std::shared_ptr<Gate>& output){
        output.reset();if(!owner||!before||GetWindowThreadProcessId(owner,nullptr)!=GetCurrentThreadId())return E_INVALIDARG;
        auto value=std::shared_ptr<Gate>(new(std::nothrow) Gate);if(!value)return E_OUTOFMEMORY;
        value->owner_=owner;value->ownerThread_=GetCurrentThreadId();value->before_=before;value->beforeContext_=beforeContext;value->resolve_=resolve;value->resolveContext_=resolveContext;value->requiresCommandIdentity_=requiresCommandIdentity;
        if(count>8192||count&&!pidls)return E_INVALIDARG;size_t total=0;
        for(UINT index=0;index<count;++index){UINT bytes=pidls[index]?ILGetSize(pidls[index]):0;if(bytes<2||bytes>65536||(total+=bytes)>16*1024*1024)return E_INVALIDARG;
            auto start=reinterpret_cast<const BYTE*>(pidls[index]);value->expected_.emplace_back(start,start+bytes);}
        WNDCLASSW type{};type.lpfnWndProc=Procedure;type.hInstance=GetModuleHandleW(nullptr);type.lpszClassName=kClass;
        if(!RegisterClassW(&type)){
            const DWORD error=GetLastError();if(error!=ERROR_CLASS_ALREADY_EXISTS)return HRESULT_FROM_WIN32(error);
            WNDCLASSW existing{};if(!GetClassInfoW(type.hInstance,kClass,&existing)||existing.lpfnWndProc!=Procedure)return E_UNEXPECTED;
        }
        auto holder=new(std::nothrow) WindowHolder{value,value->token_};if(!holder)return E_OUTOFMEMORY;
        value->dispatcher_=CreateWindowExW(0,kClass,L"",0,0,0,0,0,HWND_MESSAGE,nullptr,type.hInstance,holder);
        if(!value->dispatcher_){delete holder;return HRESULT_FROM_WIN32(GetLastError());}
        if(!SetPropW(owner,kProperty,reinterpret_cast<HANDLE>(value->token_))){const DWORD error=GetLastError();DestroyWindow(value->dispatcher_);value->dispatcher_=nullptr;return HRESULT_FROM_WIN32(error);}
        output=std::move(value);return S_OK;
    }
    ~Gate(){Invalidate();}
    ULONGLONG Token()const noexcept{return token_;}
    bool Current(ULONGLONG generation)const{std::lock_guard<std::mutex> lock(mutex_);return alive_&&generation==generation_;}
    bool Available()const{std::lock_guard<std::mutex> lock(mutex_);return alive_&&WindowMatches();}
    ULONGLONG Generation()const{std::lock_guard<std::mutex> lock(mutex_);return generation_;}
    // A native click may alias a shared command object. Remember its identity
    // without COM calls or message pumping; authorization still happens later.
    bool Select(UINT command){
        std::lock_guard<std::mutex> lock(mutex_);
        if(!command||!alive_||!armed_||GetCurrentThreadId()!=ownerThread_||!WindowMatches()||result_.phase!=Phase::Fresh)return false;
        if(selectedCommand_&&selectedCommand_!=command)return false;
        selectedCommand_=command;return true;
    }
    UINT Selected()const{std::lock_guard<std::mutex> lock(mutex_);return selectedCommand_;}
    void Generation(ULONGLONG value){
        std::vector<std::shared_ptr<Pending>> pending;
        {std::lock_guard<std::mutex> lock(mutex_);generation_=value;armed_=false;selectedCommand_=0;result_={};for(auto& item:pending_)pending.push_back(item.second);pending_.clear();}
        for(auto& item:pending){item->cancelled.store(true);SetEvent(item->event);}
    }
    void Arm(){std::lock_guard<std::mutex> lock(mutex_);if(alive_)armed_=true;}
    void Invalidate(){
        std::vector<std::shared_ptr<Pending>> pending;HWND dispatcher=nullptr,owner=nullptr;
        {std::lock_guard<std::mutex> lock(mutex_);if(!alive_&&!dispatcher_)return;alive_=false;armed_=false;before_=nullptr;beforeContext_=nullptr;resolve_=nullptr;resolveContext_=nullptr;
            for(auto& item:pending_)pending.push_back(item.second);pending_.clear();dispatcher=dispatcher_;dispatcher_=nullptr;owner=owner_;}
        for(auto& item:pending){item->cancelled.store(true);SetEvent(item->event);}
        DWORD process=0;
        if(owner&&GetWindowThreadProcessId(owner,&process)==ownerThread_&&process==GetCurrentProcessId()&&reinterpret_cast<ULONG_PTR>(GetPropW(owner,kProperty))==token_)RemovePropW(owner,kProperty);
        if(dispatcher&&GetWindowThreadProcessId(dispatcher,&process)==ownerThread_&&process==GetCurrentProcessId()){
            if(GetCurrentThreadId()==ownerThread_)DestroyWindow(dispatcher);else PostMessageW(dispatcher,kDispose,static_cast<WPARAM>(token_),0);
        }
    }
    Snapshot State()const{std::lock_guard<std::mutex> lock(mutex_);return result_;}
    void Failure(HRESULT hr,const char* stage="unspecified",const GUID* iid=nullptr,ULONGLONG expectedGeneration=~ULONGLONG(0)){
        ULONGLONG generation=0;bool changed=false;
        {std::lock_guard<std::mutex> lock(mutex_);if(alive_&&(expectedGeneration==~ULONGLONG(0)||expectedGeneration==generation_)&&FAILED(hr)&&SUCCEEDED(result_.failure)){result_.failure=hr;generation=generation_;changed=true;}}
        if(changed){
            std::fprintf(stderr,"native-command-guard failure stage=%s token=%llu generation=%llu hr=0x%08X",stage,token_,generation,static_cast<unsigned>(hr));
            if(iid)std::fprintf(stderr," iid=%08lX-%04X-%04X-%02X%02X-%02X%02X%02X%02X%02X%02X",iid->Data1,iid->Data2,iid->Data3,iid->Data4[0],iid->Data4[1],iid->Data4[2],iid->Data4[3],iid->Data4[4],iid->Data4[5],iid->Data4[6],iid->Data4[7]);
            std::fprintf(stderr,"\n");
        }
    }
    Decision Authorize(ULONGLONG generation,UINT command,IShellItemArray* selection,bool checkSelection,bool begin,bool resolve){
        Decision deniedDecision;auto request=std::make_shared<Pending>();if(!request->event){deniedDecision.status=HRESULT_FROM_WIN32(GetLastError());return deniedDecision;}
        request->generation=generation;request->command=command;request->begin=begin;request->resolve=resolve;request->checkSelection=checkSelection;
        if(selection){HRESULT hr=RoGetAgileReference(AGILEREFERENCE_DEFAULT,IID_IShellItemArray,selection,&request->selection);if(FAILED(hr)){deniedDecision.status=hr;return deniedDecision;}}
        if(GetCurrentThreadId()==ownerThread_)return SafeOnOwner(request);
        ULONGLONG id=0;HWND dispatcher=nullptr;
        {std::lock_guard<std::mutex> lock(mutex_);if(!alive_||generation!=generation_||pending_.size()>=16)return deniedDecision;id=nextRequest_++;dispatcher=dispatcher_;pending_.emplace(id,request);}
        if(!PostMessageW(dispatcher,kAuthorize,static_cast<WPARAM>(token_),static_cast<LPARAM>(id))){
            const DWORD error=GetLastError();std::lock_guard<std::mutex> lock(mutex_);pending_.erase(id);deniedDecision.status=HRESULT_FROM_WIN32(error);return deniedDecision;
        }
        DWORD index=0;HANDLE event=request->event;
        const HRESULT waited=CoWaitForMultipleHandles(COWAIT_DISPATCH_CALLS|COWAIT_DISPATCH_WINDOW_MESSAGES,5000,1,&event,&index);
        if(FAILED(waited)||request->cancelled.load()){
            request->cancelled.store(true);std::lock_guard<std::mutex> lock(mutex_);pending_.erase(id);deniedDecision.status=FAILED(waited)?waited:kCancelled;return deniedDecision;
        }
        return request->decision;
    }
    void Complete(ULONGLONG generation,HRESULT status,menu_dispatch::Kind kind=menu_dispatch::NativeHandled){
        std::lock_guard<std::mutex> lock(mutex_);if(generation!=generation_||result_.phase!=Phase::Executing)return;
        result_.phase=Phase::Completed;result_.outcome={kind,result_.command,status,menu_dispatch::Executing};++invoked;
    }
    void Handled(ULONGLONG generation,const menu_dispatch::Result& outcome){std::lock_guard<std::mutex> lock(mutex_);if(generation==generation_&&result_.phase==Phase::Fresh){result_.phase=Phase::Completed;result_.command=outcome.command;result_.outcome=outcome;}}
};

// Public service interfaces are resolved in the caller's apartment. In
// particular, an agile extension's MTA GetState must not call a raw STA view.
// The private presenter callback remains a separate owner-STA interface.
class Site final:public IServiceProvider,public IOleWindow,public IOleCommandTarget,public INewMenuClient{
    std::atomic<ULONG> refs_{1};ComPtr<IAgileReference> original_;ComPtr<IUnknown> marshaler_;std::weak_ptr<Gate> gate_;
    bool window_=false,commands_=false,newClient_=false;
    template<class T> HRESULT Get(ComPtr<T>& output)const{
        auto gate=gate_.lock();if(!gate||!gate->Available())return kCancelled;
        ComPtr<IUnknown> original;HRESULT hr=original_->Resolve(IID_PPV_ARGS(&original));return FAILED(hr)?hr:original.As(&output);
    }
public:
    static HRESULT Create(IUnknown* original,const std::shared_ptr<Gate>& gate,IServiceProvider** output){
        if(!output)return E_POINTER;*output=nullptr;if(!original||!gate)return E_INVALIDARG;
        ComPtr<Site> value;value.Attach(new(std::nothrow) Site);if(!value)return E_OUTOFMEMORY;value->gate_=gate;
        HRESULT hr=RoGetAgileReference(AGILEREFERENCE_DEFAULT,IID_IUnknown,original,&value->original_);
        if(SUCCEEDED(hr))hr=CoCreateFreeThreadedMarshaler(static_cast<IServiceProvider*>(value.Get()),&value->marshaler_);
        ComPtr<IOleWindow> window;value->window_=SUCCEEDED(original->QueryInterface(IID_PPV_ARGS(&window)));
        ComPtr<IOleCommandTarget> commands;value->commands_=SUCCEEDED(original->QueryInterface(IID_PPV_ARGS(&commands)));
        ComPtr<INewMenuClient> client;value->newClient_=SUCCEEDED(original->QueryInterface(IID_PPV_ARGS(&client)));
        if(FAILED(hr))return hr;*output=value.Detach();return S_OK;
    }
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid,void** output)override{
        if(!output)return E_POINTER;*output=nullptr;
        if(iid==IID_IUnknown||iid==IID_IServiceProvider||iid==IID_IAgileObject)*output=static_cast<IServiceProvider*>(this);
        else if(iid==IID_IMarshal)return marshaler_?marshaler_->QueryInterface(iid,output):E_NOINTERFACE;
        else if(iid==IID_IOleWindow&&window_)*output=static_cast<IOleWindow*>(this);
        else if(iid==IID_IOleCommandTarget&&commands_)*output=static_cast<IOleCommandTarget*>(this);
        else if(iid==IID_INewMenuClient&&newClient_)*output=static_cast<INewMenuClient*>(this);
        else return E_NOINTERFACE;
        AddRef();return S_OK;
    }
    ULONG STDMETHODCALLTYPE AddRef()override{return ++refs_;}
    ULONG STDMETHODCALLTYPE Release()override{ULONG count=--refs_;if(!count)delete this;return count;}
    HRESULT STDMETHODCALLTYPE QueryService(REFGUID service,REFIID iid,void** output)override{if(!output)return E_POINTER;*output=nullptr;ComPtr<IServiceProvider> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->QueryService(service,iid,output);}
    HRESULT STDMETHODCALLTYPE GetWindow(HWND* window)override{if(!window)return E_POINTER;*window=nullptr;ComPtr<IOleWindow> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->GetWindow(window);}
    HRESULT STDMETHODCALLTYPE ContextSensitiveHelp(BOOL value)override{ComPtr<IOleWindow> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->ContextSensitiveHelp(value);}
    HRESULT STDMETHODCALLTYPE QueryStatus(const GUID* group,ULONG count,OLECMD* commands,OLECMDTEXT* text)override{ComPtr<IOleCommandTarget> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->QueryStatus(group,count,commands,text);}
    HRESULT STDMETHODCALLTYPE Exec(const GUID* group,DWORD command,DWORD options,VARIANT* input,VARIANT* output)override{ComPtr<IOleCommandTarget> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->Exec(group,command,options,input,output);}
    HRESULT STDMETHODCALLTYPE IncludeItems(NMCII_FLAGS* flags)override{ComPtr<INewMenuClient> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->IncludeItems(flags);}
    HRESULT STDMETHODCALLTYPE SelectAndEditItem(PCIDLIST_ABSOLUTE item,NMCSAEI_FLAGS flags)override{ComPtr<INewMenuClient> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->SelectAndEditItem(item,flags);}
};

inline constexpr GUID kProxyIdentity={0xc85481c2,0xe043,0x4d9b,{0x9a,0xaf,0x88,0x4c,0xc7,0xf0,0xc2,0xb1}};
// Read-only package identity metadata used by the verified native presenter.
// The matching Shell adapter's fourth slot receives the same selection as
// GetIcon and returns a caller-owned CoTaskMem PWSTR. A missing capability or
// failed getter has a native empty-string fallback; it is not an Invoke path.
struct __declspec(uuid("17CF0D4C-AFE5-48AE-9B38-3BD282067169")) PackageNameMetadata:IUnknown{
    virtual HRESULT STDMETHODCALLTYPE GetPackageFullName(IShellItemArray*,PWSTR*)=0;
};
struct ProxyIdentity:IUnknown {
    virtual HRESULT STDMETHODCALLTYPE Identity(ULONGLONG*,ULONGLONG*)=0;
    virtual HRESULT STDMETHODCALLTYPE BindCommand(UINT)=0;
    // Compare identity without handing the caller an unguarded command.
    virtual HRESULT STDMETHODCALLTYPE HasAncestor(ULONG,IAgileReference* const*,BOOL*)=0;
};
using Ancestors=std::vector<ComPtr<IAgileReference>>;
class Command;
class Enumeration;
inline HRESULT WrapCommand(IExplorerCommand*,const std::shared_ptr<Gate>&,ULONGLONG,const Ancestors&,IExplorerCommand**);
inline HRESULT WrapEnumeration(IEnumExplorerCommand*,const std::shared_ptr<Gate>&,ULONGLONG,const Ancestors&,IEnumExplorerCommand**);

class Command final:public IExplorerCommand,public IObjectWithSite,public IObjectWithSelection,
        public IInitializeCommand,public IInitializeWithBindCtx,public IExplorerCommandState,public PackageNameMetadata,public ProxyIdentity{
    std::atomic<ULONG> refs_{1};ComPtr<IAgileReference> original_;ComPtr<IUnknown> marshaler_;
    std::weak_ptr<Gate> gate_;ULONGLONG token_=0,generation_=0;Ancestors ancestors_;
    bool site_=false,selection_=false,initialize_=false,bind_=false,state_=false,packageName_=false;
    mutable std::mutex idsMutex_;std::vector<UINT> commandIds_;
    template<class T> HRESULT Get(ComPtr<T>& output)const{auto gate=gate_.lock();if(!gate||!gate->Current(generation_))return kCancelled;ComPtr<IExplorerCommand> command;HRESULT hr=original_->Resolve(IID_PPV_ARGS(&command));return FAILED(hr)?hr:command.As(&output);}
    ~Command()=default;
public:
    static HRESULT Create(IExplorerCommand* original,const std::shared_ptr<Gate>& gate,ULONGLONG generation,const Ancestors& ancestors,IExplorerCommand** output){
        if(!output)return E_POINTER;*output=nullptr;if(!original||!gate)return E_INVALIDARG;
        ComPtr<Command> value;value.Attach(new(std::nothrow) Command);if(!value)return E_OUTOFMEMORY;
        value->gate_=gate;value->token_=gate->Token();value->generation_=generation;value->ancestors_=ancestors;
        HRESULT hr=RoGetAgileReference(AGILEREFERENCE_DEFAULT,IID_IExplorerCommand,original,&value->original_);
        if(SUCCEEDED(hr))hr=CoCreateFreeThreadedMarshaler(static_cast<IExplorerCommand*>(value.Get()),&value->marshaler_);
        ComPtr<IObjectWithSite> site;value->site_=SUCCEEDED(original->QueryInterface(IID_PPV_ARGS(&site)));
        ComPtr<IObjectWithSelection> selection;value->selection_=SUCCEEDED(original->QueryInterface(IID_PPV_ARGS(&selection)));
        ComPtr<IInitializeCommand> initialize;value->initialize_=SUCCEEDED(original->QueryInterface(IID_PPV_ARGS(&initialize)));
        ComPtr<IInitializeWithBindCtx> bind;value->bind_=SUCCEEDED(original->QueryInterface(IID_PPV_ARGS(&bind)));
        ComPtr<IExplorerCommandState> state;value->state_=SUCCEEDED(original->QueryInterface(IID_PPV_ARGS(&state)));
        ComPtr<PackageNameMetadata> packageName;value->packageName_=SUCCEEDED(original->QueryInterface(IID_PPV_ARGS(&packageName)));
        if(FAILED(hr))return hr;value->ancestors_.push_back(value->original_);*output=value.Detach();++wrapped;return S_OK;
    }
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid,void** output)override{
        if(!output)return E_POINTER;*output=nullptr;
        if(iid==IID_IUnknown||iid==IID_IExplorerCommand||iid==IID_IAgileObject)*output=static_cast<IExplorerCommand*>(this);
        else if(iid==IID_IMarshal)return marshaler_?marshaler_->QueryInterface(iid,output):E_NOINTERFACE;
        else if(iid==kProxyIdentity)*output=static_cast<ProxyIdentity*>(this);
        else if(iid==IID_IObjectWithSite&&site_)*output=static_cast<IObjectWithSite*>(this);
        else if(iid==IID_IObjectWithSelection&&selection_)*output=static_cast<IObjectWithSelection*>(this);
        else if(iid==IID_IInitializeCommand&&initialize_)*output=static_cast<IInitializeCommand*>(this);
        else if(iid==IID_IInitializeWithBindCtx&&bind_)*output=static_cast<IInitializeWithBindCtx*>(this);
        else if(iid==IID_IExplorerCommandState&&state_)*output=static_cast<IExplorerCommandState*>(this);
        else if(iid==__uuidof(PackageNameMetadata)){
            if(!packageName_)return E_NOINTERFACE;
            *output=static_cast<PackageNameMetadata*>(this);
        }
        else{
            ComPtr<IExplorerCommand> original;ComPtr<IUnknown> unknown;
            if(SUCCEEDED(Get(original))&&SUCCEEDED(original->QueryInterface(iid,reinterpret_cast<void**>(unknown.GetAddressOf())))){
                RecordUnsupported(iid);
            }
            // This proxy does not implement the queried capability. Returning
            // E_NOINTERFACE is a normal COM answer, not an execution failure.
            // Never expose the unguarded original interface; the actual
            // consumer decides whether absence is optional or required.
            return E_NOINTERFACE;
        }
        AddRef();return S_OK;
    }
    ULONG STDMETHODCALLTYPE AddRef()override{return ++refs_;}
    ULONG STDMETHODCALLTYPE Release()override{ULONG count=--refs_;if(!count)delete this;return count;}
    HRESULT STDMETHODCALLTYPE Identity(ULONGLONG* token,ULONGLONG* generation)override{if(!token||!generation)return E_POINTER;*token=token_;*generation=generation_;return S_OK;}
    HRESULT STDMETHODCALLTYPE HasAncestor(ULONG count,IAgileReference* const* ancestors,BOOL* found)override{
        if(!found)return E_POINTER;*found=FALSE;if(count&&!ancestors)return E_INVALIDARG;
        if(count>=64)return HRESULT_FROM_WIN32(ERROR_STACK_OVERFLOW);
        ComPtr<IUnknown> identity;HRESULT hr=Get(identity);if(FAILED(hr))return hr;
        for(ULONG index=0;index<count;++index){
            if(!ancestors[index])return E_INVALIDARG;
            ComPtr<IUnknown> previous;hr=ancestors[index]->Resolve(IID_PPV_ARGS(&previous));if(FAILED(hr))return hr;
            if(previous==identity){*found=TRUE;break;}
        }
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE BindCommand(UINT command)override{
        if(!command)return E_INVALIDARG;std::lock_guard<std::mutex> lock(idsMutex_);
        if(std::find(commandIds_.begin(),commandIds_.end(),command)!=commandIds_.end())return S_OK;
        if(commandIds_.size()>=256){if(auto gate=gate_.lock())gate->Failure(E_OUTOFMEMORY,"command-id-limit",nullptr,generation_);return E_OUTOFMEMORY;}
        commandIds_.push_back(command);return S_OK;
    }
    HRESULT STDMETHODCALLTYPE GetTitle(IShellItemArray* items,LPWSTR* value)override{ComPtr<IExplorerCommand> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->GetTitle(items,value);}
    HRESULT STDMETHODCALLTYPE GetIcon(IShellItemArray* items,LPWSTR* value)override{ComPtr<IExplorerCommand> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->GetIcon(items,value);}
    HRESULT STDMETHODCALLTYPE GetToolTip(IShellItemArray* items,LPWSTR* value)override{ComPtr<IExplorerCommand> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->GetToolTip(items,value);}
    HRESULT STDMETHODCALLTYPE GetCanonicalName(GUID* value)override{ComPtr<IExplorerCommand> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->GetCanonicalName(value);}
    HRESULT STDMETHODCALLTYPE GetState(IShellItemArray* items,BOOL slow,EXPCMDSTATE* value)override{ComPtr<IExplorerCommand> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->GetState(items,slow,value);}
    HRESULT STDMETHODCALLTYPE GetFlags(EXPCMDFLAGS* value)override{ComPtr<IExplorerCommand> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->GetFlags(value);}
    HRESULT STDMETHODCALLTYPE GetPackageFullName(IShellItemArray* items,PWSTR* value)override{
        if(!value)return E_POINTER;*value=nullptr;
        ComPtr<PackageNameMetadata> original;HRESULT hr=Get(original);
        // Transfer the original allocation and HRESULT unchanged. The native
        // consumer owns CoTaskMemFree; no command authorization is involved.
        return FAILED(hr)?hr:original->GetPackageFullName(items,value);
    }
    HRESULT STDMETHODCALLTYPE Invoke(IShellItemArray* items,IBindCtx* context)override{
        auto gate=gate_.lock();if(!gate||!gate->Current(generation_))return kCancelled;
        HRESULT hr=E_UNEXPECTED;bool executing=false;
        try{
            ComPtr<IExplorerCommand> original;hr=Get(original);if(FAILED(hr))return hr;
            const auto state=gate->State();UINT command=0;
            {std::lock_guard<std::mutex> lock(idsMutex_);
                if(state.phase==Phase::Granted){
                    if(!state.command||std::find(commandIds_.begin(),commandIds_.end(),state.command)==commandIds_.end())return kCancelled;
                    command=state.command;
                }else if(const auto selected=gate->Selected()){
                    if(std::find(commandIds_.begin(),commandIds_.end(),selected)==commandIds_.end())return kCancelled;
                    command=selected;
                }else if(commandIds_.size()==1)command=commandIds_[0];
                else if(commandIds_.size()>1){gate->Failure(E_NOTIMPL,"ambiguous-command-id",nullptr,generation_);return E_NOTIMPL;}
            }
            const Decision decision=gate->Authorize(generation_,command,items,true,true,true);
            if(FAILED(decision.status)||!decision.execute)return decision.status;
            executing=true;hr=original->Invoke(items,context);
        }catch(const std::bad_alloc&){hr=E_OUTOFMEMORY;}catch(...){hr=E_UNEXPECTED;}
        if(executing)gate->Complete(generation_,hr);else gate->Failure(hr,"invoke-setup",nullptr,generation_);return hr;
    }
    HRESULT STDMETHODCALLTYPE EnumSubCommands(IEnumExplorerCommand** output)override{
        if(!output)return E_POINTER;*output=nullptr;auto gate=gate_.lock();if(!gate||!gate->Current(generation_))return kCancelled;
        ComPtr<IExplorerCommand> original;ComPtr<IEnumExplorerCommand> enumerator;HRESULT hr=Get(original);if(SUCCEEDED(hr))hr=original->EnumSubCommands(&enumerator);
        // E_NOTIMPL/E_NOINTERFACE are normal for leaf commands. Preserve the
        // provider result; only our inability to guard returned children fails
        // the host's preparation contract.
        if(FAILED(hr)||!enumerator)return hr;
        hr=WrapEnumeration(enumerator.Get(),gate,generation_,ancestors_,output);
        if(FAILED(hr))gate->Failure(hr,"wrap-subcommands",&IID_IEnumExplorerCommand,generation_);return hr;
    }
    HRESULT STDMETHODCALLTYPE SetSite(IUnknown* site)override{ComPtr<IObjectWithSite> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->SetSite(site);}
    HRESULT STDMETHODCALLTYPE GetSite(REFIID iid,void** output)override{ComPtr<IObjectWithSite> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->GetSite(iid,output);}
    HRESULT STDMETHODCALLTYPE SetSelection(IShellItemArray* items)override{ComPtr<IObjectWithSelection> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->SetSelection(items);}
    HRESULT STDMETHODCALLTYPE GetSelection(REFIID iid,void** output)override{ComPtr<IObjectWithSelection> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->GetSelection(iid,output);}
    HRESULT STDMETHODCALLTYPE Initialize(LPCWSTR name,IPropertyBag* bag)override{ComPtr<IInitializeCommand> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->Initialize(name,bag);}
    HRESULT STDMETHODCALLTYPE Initialize(IBindCtx* context)override{ComPtr<IInitializeWithBindCtx> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->Initialize(context);}
};

class Enumeration final:public IEnumExplorerCommand{
    std::atomic<ULONG> refs_{1};ComPtr<IAgileReference> original_;ComPtr<IUnknown> marshaler_;
    std::weak_ptr<Gate> gate_;ULONGLONG generation_=0;Ancestors ancestors_;
    HRESULT Get(ComPtr<IEnumExplorerCommand>& output){return original_->Resolve(IID_PPV_ARGS(&output));}
public:
    static HRESULT Create(IEnumExplorerCommand* original,const std::shared_ptr<Gate>& gate,ULONGLONG generation,const Ancestors& ancestors,IEnumExplorerCommand** output){
        if(!output)return E_POINTER;*output=nullptr;ComPtr<Enumeration> value;value.Attach(new(std::nothrow) Enumeration);if(!value)return E_OUTOFMEMORY;
        value->gate_=gate;value->generation_=generation;value->ancestors_=ancestors;
        HRESULT hr=RoGetAgileReference(AGILEREFERENCE_DEFAULT,IID_IEnumExplorerCommand,original,&value->original_);
        if(SUCCEEDED(hr))hr=CoCreateFreeThreadedMarshaler(static_cast<IEnumExplorerCommand*>(value.Get()),&value->marshaler_);
        if(FAILED(hr))return hr;*output=value.Detach();return S_OK;
    }
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid,void** output)override{if(!output)return E_POINTER;*output=nullptr;
        if(iid==IID_IUnknown||iid==IID_IEnumExplorerCommand||iid==IID_IAgileObject){*output=static_cast<IEnumExplorerCommand*>(this);AddRef();return S_OK;}
        if(iid==IID_IMarshal)return marshaler_?marshaler_->QueryInterface(iid,output):E_NOINTERFACE;return E_NOINTERFACE;}
    ULONG STDMETHODCALLTYPE AddRef()override{return ++refs_;}
    ULONG STDMETHODCALLTYPE Release()override{ULONG count=--refs_;if(!count)delete this;return count;}
    HRESULT STDMETHODCALLTYPE Next(ULONG count,IExplorerCommand** values,ULONG* fetched)override{
        if(fetched)*fetched=0;if(!values||(!fetched&&count!=1)||count>8192)return E_INVALIDARG;for(ULONG i=0;i<count;++i)values[i]=nullptr;
        auto gate=gate_.lock();if(!gate||!gate->Current(generation_))return kCancelled;
        ComPtr<IEnumExplorerCommand> original;HRESULT hr=Get(original);if(FAILED(hr))return hr;
        std::vector<IExplorerCommand*> raw(count,nullptr);ULONG received=0;hr=original->Next(count,raw.data(),&received);const HRESULT originalResult=hr;
        if(SUCCEEDED(hr)&&received<=count){for(ULONG i=0;i<received;++i){hr=WrapCommand(raw[i],gate,generation_,ancestors_,&values[i]);if(FAILED(hr))break;}}
        else if(SUCCEEDED(hr))hr=E_UNEXPECTED;
        for(auto item:raw)if(item)item->Release();
        if(FAILED(hr)){for(ULONG i=0;i<count;++i){if(values[i])values[i]->Release();values[i]=nullptr;}gate->Failure(hr,"wrap-enumerated-command",&IID_IExplorerCommand,generation_);return hr;}
        if(fetched)*fetched=received;return originalResult;
    }
    HRESULT STDMETHODCALLTYPE Skip(ULONG count)override{auto gate=gate_.lock();if(!gate||!gate->Current(generation_))return kCancelled;ComPtr<IEnumExplorerCommand> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->Skip(count);}
    HRESULT STDMETHODCALLTYPE Reset()override{auto gate=gate_.lock();if(!gate||!gate->Current(generation_))return kCancelled;ComPtr<IEnumExplorerCommand> original;HRESULT hr=Get(original);return FAILED(hr)?hr:original->Reset();}
    HRESULT STDMETHODCALLTYPE Clone(IEnumExplorerCommand** output)override{if(!output)return E_POINTER;*output=nullptr;auto gate=gate_.lock();if(!gate||!gate->Current(generation_))return kCancelled;
        ComPtr<IEnumExplorerCommand> original,copy;HRESULT hr=Get(original);if(SUCCEEDED(hr))hr=original->Clone(&copy);return FAILED(hr)?hr:WrapEnumeration(copy.Get(),gate,generation_,ancestors_,output);}
};

inline HRESULT WrapCommand(IExplorerCommand* original,const std::shared_ptr<Gate>& gate,ULONGLONG generation,const Ancestors& ancestors,IExplorerCommand** output){
    if(!output)return E_POINTER;*output=nullptr;if(!original||!gate||!gate->Current(generation))return kCancelled;
    ComPtr<ProxyIdentity> wrappedIdentity;
    if(SUCCEEDED(original->QueryInterface(kProxyIdentity,reinterpret_cast<void**>(wrappedIdentity.GetAddressOf())))){
        ULONGLONG token=0,oldGeneration=0;HRESULT hr=wrappedIdentity->Identity(&token,&oldGeneration);
        if(FAILED(hr)||token!=gate->Token()||oldGeneration!=generation)return kCancelled;
        if(ancestors.size()>=64)return HRESULT_FROM_WIN32(ERROR_STACK_OVERFLOW);
        std::vector<IAgileReference*> chain;chain.reserve(ancestors.size());
        for(const auto& ancestor:ancestors)chain.push_back(ancestor.Get());
        BOOL found=FALSE;hr=wrappedIdentity->HasAncestor(static_cast<ULONG>(chain.size()),chain.data(),&found);
        if(FAILED(hr))return hr;if(found)return HRESULT_FROM_WIN32(ERROR_CIRCULAR_DEPENDENCY);
        return original->QueryInterface(IID_PPV_ARGS(output));
    }
    ComPtr<IUnknown> identity;HRESULT hr=original->QueryInterface(IID_PPV_ARGS(&identity));if(FAILED(hr))return hr;
    if(ancestors.size()>=64)return HRESULT_FROM_WIN32(ERROR_STACK_OVERFLOW);
    for(const auto& ancestor:ancestors){ComPtr<IUnknown> previous;hr=ancestor->Resolve(IID_PPV_ARGS(&previous));if(FAILED(hr))return hr;if(previous==identity)return HRESULT_FROM_WIN32(ERROR_CIRCULAR_DEPENDENCY);}
    return Command::Create(original,gate,generation,ancestors,output);
}
inline HRESULT WrapEnumeration(IEnumExplorerCommand* original,const std::shared_ptr<Gate>& gate,ULONGLONG generation,const Ancestors& ancestors,IEnumExplorerCommand** output){
    if(!original||!gate||!gate->Current(generation))return kCancelled;return Enumeration::Create(original,gate,generation,ancestors,output);
}
} // namespace native_command_guard
