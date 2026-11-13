// Windows 11 context menu service for an application-owned ExplorerBrowser.
// The private ABI is enabled only for the verified system DLL; see THIRD-PARTY-NOTICES.
#include <windows.h>
#include <bcrypt.h>
#include <string>
#include <vector>
#include <iterator>
#include <cstring>
#include <shlobj.h>
#include <shobjidl.h>
#include <shlguid.h>
#include <servprov.h>
#include <commctrl.h>
#include <docobj.h>
#include <inspectable.h>
#include <new>
#include <atomic>
#include <unordered_map>
#include <unordered_set>
#include <cstdint>
#include <strsafe.h>
#include "MenuSelectionSnapshot.h"
#include "MenuCommandBridge.h"
#include "MenuCommandDispatch.h"
#include "MixedItemMenu.h"
#include "MenuModelFilter.h"

// Installed only by an isolated menu worker. Record a last-chance fault without
// attempting to continue in a process whose native extension state is corrupt.
static HANDLE gFaultReport=INVALID_HANDLE_VALUE;
static LPTOP_LEVEL_EXCEPTION_FILTER gPreviousFaultFilter=nullptr;
static volatile LONG gFaultReporting=0;
static LONG WINAPI ReportMenuFault(EXCEPTION_POINTERS* pointers) noexcept {
    if(InterlockedCompareExchange(&gFaultReporting,1,0)!=0)return EXCEPTION_CONTINUE_SEARCH;
    if(gFaultReport!=INVALID_HANDLE_VALUE&&pointers&&pointers->ExceptionRecord){
        const auto record=pointers->ExceptionRecord;
        HMODULE module=nullptr;
        const bool known=GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS|GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            reinterpret_cast<LPCWSTR>(record->ExceptionAddress),&module)!=FALSE;
        wchar_t fullName[1024]{};char name[768]="unavailable",escaped[1536]{};
        const DWORD length=known?GetModuleFileNameW(module,fullName,static_cast<DWORD>(std::size(fullName))):0;
        if(length&&length<std::size(fullName)){
            const wchar_t* base=fullName;
            for(const wchar_t* current=fullName;*current;++current)if(*current==L'\\'||*current==L'/')base=current+1;
            if(!WideCharToMultiByte(CP_UTF8,0,base,-1,name,static_cast<int>(std::size(name)),nullptr,nullptr))
                StringCchCopyA(name,std::size(name),"unavailable");
        }
        size_t next=0;
        for(size_t index=0;name[index]&&next+2<std::size(escaped);++index){
            const unsigned char value=static_cast<unsigned char>(name[index]);
            if(value=='"'||value=='\\')escaped[next++]='\\';
            escaped[next++]=value<32?'?':static_cast<char>(value);
        }
        char json[2048]{};
        const auto address=reinterpret_cast<ULONG_PTR>(record->ExceptionAddress);
        const auto offset=known?address-reinterpret_cast<ULONG_PTR>(module):0;
        if(SUCCEEDED(StringCchPrintfA(json,std::size(json),
                "{\"exception_code\":\"0x%08lX\",\"address\":\"0x%llX\",\"module\":\"%s\",\"module_known\":%s,\"module_offset\":\"0x%llX\"}\n",
                record->ExceptionCode,static_cast<unsigned long long>(address),escaped,known?"true":"false",static_cast<unsigned long long>(offset)))){
            DWORD written=0;WriteFile(gFaultReport,json,static_cast<DWORD>(strlen(json)),&written,nullptr);FlushFileBuffers(gFaultReport);
        }
    }
    const auto previous=gPreviousFaultFilter;
    if(previous&&previous!=ReportMenuFault)previous(pointers);
    return EXCEPTION_CONTINUE_SEARCH;
}
extern "C" HRESULT zs_menu_install_fault_report(PCWSTR path) noexcept {
    if(!path||!*path)return E_INVALIDARG;
    if(gFaultReport!=INVALID_HANDLE_VALUE)return S_OK;
    HANDLE file=CreateFileW(path,GENERIC_WRITE,FILE_SHARE_READ|FILE_SHARE_DELETE,nullptr,CREATE_ALWAYS,
        FILE_ATTRIBUTE_NORMAL|FILE_FLAG_WRITE_THROUGH,nullptr);
    if(file==INVALID_HANDLE_VALUE)return HRESULT_FROM_WIN32(GetLastError());
    gFaultReport=file;gPreviousFaultFilter=SetUnhandledExceptionFilter(ReportMenuFault);return S_OK;
}

extern "C" HRESULT zs_menu_for_mixed_items(HWND owner,UINT count,PCIDLIST_ABSOLUTE const* pidls,IContextMenu** out) noexcept {
    try{return mixed_menu::Create(owner,count,pidls,out);}
    catch(const std::bad_alloc&){return E_OUTOFMEMORY;}
    catch(...){return E_UNEXPECTED;}
}

static bool FileSha256(const wchar_t* path, std::string& digest) {
    HANDLE file = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                             nullptr, OPEN_EXISTING, FILE_FLAG_SEQUENTIAL_SCAN, nullptr);
    if (file == INVALID_HANDLE_VALUE) return false;
    BCRYPT_ALG_HANDLE algorithm = nullptr;
    BCRYPT_HASH_HANDLE hash = nullptr;
    bool good = BCryptOpenAlgorithmProvider(&algorithm, BCRYPT_SHA256_ALGORITHM, nullptr, 0) >= 0;
    DWORD objectSize = 0, returned = 0;
    if (good) good = BCryptGetProperty(algorithm, BCRYPT_OBJECT_LENGTH, reinterpret_cast<PUCHAR>(&objectSize), sizeof(objectSize), &returned, 0) >= 0;
    std::vector<BYTE> object(objectSize), buffer(65536);
    BYTE bytes[32]{};
    if (good) good = BCryptCreateHash(algorithm, &hash, object.data(), objectSize, nullptr, 0, 0) >= 0;
    while (good) {
        DWORD count = 0;
        if (!ReadFile(file, buffer.data(), static_cast<DWORD>(buffer.size()), &count, nullptr)) { good = false; break; }
        if (!count) break;
        good = BCryptHashData(hash, buffer.data(), count, 0) >= 0;
    }
    if (good) good = BCryptFinishHash(hash, bytes, sizeof(bytes), 0) >= 0;
    if (hash) BCryptDestroyHash(hash);
    if (algorithm) BCryptCloseAlgorithmProvider(algorithm, 0);
    CloseHandle(file);
    if (!good) return false;
    static const char digits[] = "0123456789abcdef";
    digest.clear(); digest.reserve(64);
    for (BYTE byte : bytes) { digest.push_back(digits[byte >> 4]); digest.push_back(digits[byte & 15]); }
    return true;
}
static const GUID kPresenterClass = {0x86ca1aa0,0x34aa,0x4e8b,{0xa5,0x09,0x50,0xc9,0x05,0xba,0xe2,0xa2}};
static const GUID kPresenterIID = {0x37a472f7,0x63cf,0x4ccf,{0xa8,0x8b,0x52,0x31,0xa3,0xc7,0xd8,0xb6}};
static const GUID kPresenterSID = {0xb306c5b1,0xb4f2,0x473c,{0xb6,0xff,0x70,0x1b,0x24,0x6c,0xe2,0xd2}};
static const GUID kCallbackIID = {0x9a19ddcf,0x9ed4,0x4a18,{0x89,0xde,0xc3,0xb4,0xdd,0x1d,0x7a,0xe3}};
static const GUID kClosableIID = {0x30d5a829,0x7fa4,0x4026,{0x83,0xbb,0xd7,0x5b,0xae,0x4e,0xa9,0x9e}};
struct NativeClosable : IInspectable { virtual HRESULT STDMETHODCALLTYPE Close()=0; };
struct Presenter : IUnknown {
    virtual HRESULT STDMETHODCALLTYPE Initialize(BOOL,IUnknown*,HWND,int)=0;
    virtual HRESULT STDMETHODCALLTYPE PrepareForContextMenu(UINT,IUnknown*,POINT,IContextMenu*,UINT,UINT,BOOL,const void*,GUID,IShellItem*)=0;
    virtual void STDMETHODCALLTYPE DoContextMenu(HMENU,UINT,GUID)=0;
    virtual void STDMETHODCALLTYPE DismissContextMenu(PCWSTR)=0;
    virtual BOOL STDMETHODCALLTYPE IsContextMenuOpen()=0;
    virtual void STDMETHODCALLTYPE Invoke(UINT)=0;
};
struct NativeCallback : IUnknown {
    virtual void STDMETHODCALLTYPE Invoke(IContextMenu*,IShellItemArray*,HMENU,UINT,POINT)=0;
    virtual void STDMETHODCALLTYPE OnContextMenuDismiss()=0;
    virtual void STDMETHODCALLTYPE DoExpandedContextMenu(POINT,int,GUID*,POINT*,int)=0;
    virtual void STDMETHODCALLTYPE SetFocus()=0;
};

class Session;
using NativePrepareFn=HRESULT(STDMETHODCALLTYPE*)(Presenter*,UINT,IUnknown*,POINT,IContextMenu*,UINT,UINT,BOOL,const void*,GUID,IShellItem*);
static std::atomic<NativePrepareFn> gNativePrepare{nullptr};
static HRESULT STDMETHODCALLTYPE PrepareAgile(Presenter*,UINT,IUnknown*,POINT,IContextMenu*,UINT,UINT,BOOL,const void*,GUID,IShellItem*);
struct PresenterRecord { Session* session;DWORD thread;ULONGLONG generation;uint32_t hidden;HMENU extra=nullptr;bool extraPending=false; };
static SRWLOCK gPresenterLock=SRWLOCK_INIT;
static std::unordered_map<IUnknown*,PresenterRecord> gPresenters;
static std::atomic<ULONGLONG> gRequests{0},gPresenterCreations{0},gSelectionSnapshots{0},gRuntimeInitializations{0};
static thread_local ULONGLONG gLastQueryMicros=0,gLastPrepareMicros=0,gLastPresenterCreateMicros=0;
static thread_local ULONGLONG gCommandMessages=0;
static thread_local bool gNativeCloseFault=false;
static thread_local uint32_t gHiddenMenuFeatures=menu_model_filter::Wps;
using OriginalMenuCallback=HRESULT(WINAPI*)(IShellView*,POINT);
static std::atomic<OriginalMenuCallback> gOriginalMenuCallback{nullptr};
static ULONGLONG Micros(){LARGE_INTEGER time{},frequency{};QueryPerformanceCounter(&time);QueryPerformanceFrequency(&frequency);const auto ticks=static_cast<ULONGLONG>(time.QuadPart),rate=static_cast<ULONGLONG>(frequency.QuadPart);return (ticks/rate)*1000000+(ticks%rate)*1000000/rate;}
static bool CommandPresenterOwner(IUnknown* native,DWORD* thread,ULONGLONG* generation) noexcept {
    if(!native||!thread||!generation)return false;
    AcquireSRWLockShared(&gPresenterLock);auto found=gPresenters.find(native);bool valid=found!=gPresenters.end();
    if(valid){*thread=found->second.thread;*generation=found->second.generation;}
    ReleaseSRWLockShared(&gPresenterLock);return valid;
}
static bool PresenterMenuPolicy(IUnknown* native,uint32_t* hidden) noexcept {
    if(!native||!hidden)return false;
    AcquireSRWLockShared(&gPresenterLock);auto found=gPresenters.find(native);
    const bool valid=found!=gPresenters.end();if(valid)*hidden=found->second.hidden;
    ReleaseSRWLockShared(&gPresenterLock);return valid;
}
static HRESULT AppendStandaloneModels(IUnknown* native,IUnknown** output) noexcept {
    HMENU extra=nullptr;
    AcquireSRWLockExclusive(&gPresenterLock);auto found=gPresenters.find(native);
    if(found!=gPresenters.end()&&found->second.thread==GetCurrentThreadId()&&found->second.extraPending){
        extra=found->second.extra;found->second.extraPending=false;
    }
    ReleaseSRWLockExclusive(&gPresenterLock);
    if(!extra||!output||!*output)return S_FALSE;
    try{
        Microsoft::WRL::ComPtr<IUnknown> items;
        menu_model_filter::originalCreate(native,items.GetAddressOf(),extra);
        Microsoft::WRL::ComPtr<menu_model_filter::ItemVector> source,destination;
        HRESULT result=items?items->QueryInterface(menu_model_filter::itemVectorIid,reinterpret_cast<void**>(source.GetAddressOf())):E_UNEXPECTED;
        if(SUCCEEDED(result))result=(*output)->QueryInterface(menu_model_filter::itemVectorIid,reinterpret_cast<void**>(destination.GetAddressOf()));
        UINT count=0;if(SUCCEEDED(result))result=source->Size(&count);
        std::unordered_set<std::wstring> ownIds;
        for(UINT index=0;SUCCEEDED(result)&&index<count;++index){
            Microsoft::WRL::ComPtr<IInspectable> item;result=source->GetAt(index,&item);
            menu_model_filter::String id;if(SUCCEEDED(result))result=menu_model_filter::GetId(item.Get(),id);
            if(SUCCEEDED(result)&&!menu_model_filter::Equal(id.View(),L"Separator"))ownIds.emplace(id.View());
        }
        UINT existing=0;if(SUCCEEDED(result))result=destination->Size(&existing);
        for(UINT index=existing;SUCCEEDED(result)&&index>0;--index){
            Microsoft::WRL::ComPtr<IInspectable> item;result=destination->GetAt(index-1,&item);
            menu_model_filter::String id;if(SUCCEEDED(result))result=menu_model_filter::GetId(item.Get(),id);
            if(SUCCEEDED(result)&&ownIds.count(std::wstring(id.View())))result=destination->RemoveAt(index-1);
        }
        // Background menus may already contain the application section.
        // Replace it by identity, retaining one copy and one group separator.
        while(SUCCEEDED(result)){
            result=destination->Size(&existing);if(FAILED(result)||!existing)break;
            Microsoft::WRL::ComPtr<IInspectable> item;result=destination->GetAt(0,&item);
            menu_model_filter::String id;if(SUCCEEDED(result))result=menu_model_filter::GetId(item.Get(),id);
            if(FAILED(result)||!menu_model_filter::Equal(id.View(),L"Separator"))break;
            result=destination->RemoveAt(0);
        }
        for(UINT index=count;SUCCEEDED(result)&&index>0;--index){
            Microsoft::WRL::ComPtr<IInspectable> item;result=source->GetAt(index-1,&item);
            if(SUCCEEDED(result))result=destination->InsertAt(0,item.Get());
        }
        return result;
    }catch(...){return E_UNEXPECTED;}
}
static HMODULE gMenuModule=nullptr;
static void** gIdentitySlot=nullptr;
static void* gOriginalIdentity=nullptr;
static SRWLOCK gIdentityLock=SRWLOCK_INIT;
static volatile LONG gLastFailure=S_OK, gRuntimePins=0;
static std::atomic<unsigned> gSharedHosts{0};
struct RuntimeOwner { DWORD thread=0;HMODULE module=nullptr;IUnknown* controller=nullptr;IUnknown* queue=nullptr;IUnknown* manager=nullptr;HRESULT status=E_PENDING; };
// Native framework objects have no adapter callbacks. Retain one runtime owner
// per UI thread until application exit; rollback restores behavior without
// shutting down a dispatcher shared by the application's UI thread.
static thread_local RuntimeOwner* gThreadRuntime=nullptr;
static constexpr char kExpectedMenuSha256[]="4ce847cbbe84bedab2799bc7de9aa1d85684e66b95e2c59daf4ecee759aef0b1";
static HRESULT LastErrorResult() { DWORD value=GetLastError(); return HRESULT_FROM_WIN32(value?value:ERROR_GEN_FAILURE); }
static DWORD CurrentBuild() {
    using VersionFn=LONG(WINAPI*)(OSVERSIONINFOW*);
    auto function=reinterpret_cast<VersionFn>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"),"RtlGetVersion"));
    OSVERSIONINFOW version{};version.dwOSVersionInfoSize=sizeof(version);
    return function && function(&version)>=0 ? version.dwBuildNumber : 0;
}
static bool ExchangeGuarded(void** slot,void* expected,void* replacement) {
    DWORD oldProtection=0;
    if(!VirtualProtect(slot,sizeof(void*),PAGE_READWRITE,&oldProtection))return false;
    void* actual=InterlockedCompareExchangePointer(slot,replacement,expected);
    DWORD ignored=0;
    if(!VirtualProtect(slot,sizeof(void*),oldProtection,&ignored))InterlockedExchange(&gLastFailure,LastErrorResult());
    return actual==expected;
}
static BOOL WINAPI ExplorerIdentityForMenuModule() { return TRUE; }
static HRESULT EnsureIdentityCompatibility() {
    AcquireSRWLockExclusive(&gIdentityLock);
    if(gIdentitySlot){ReleaseSRWLockExclusive(&gIdentityLock);return S_OK;}
    HMODULE module=gMenuModule;
    const bool acquired=module==nullptr;
    if(!module&&!GetModuleHandleExW(0,L"Windows.UI.FileExplorer.dll",&module)){ReleaseSRWLockExclusive(&gIdentityLock);return LastErrorResult();}
    BYTE* base=reinterpret_cast<BYTE*>(module);
    auto dos=reinterpret_cast<IMAGE_DOS_HEADER*>(base);
    auto nt=reinterpret_cast<IMAGE_NT_HEADERS64*>(base+dos->e_lfanew);
    HRESULT result=HRESULT_FROM_WIN32(ERROR_PROC_NOT_FOUND);
    if(dos->e_magic==IMAGE_DOS_SIGNATURE && nt->Signature==IMAGE_NT_SIGNATURE && nt->OptionalHeader.Magic==IMAGE_NT_OPTIONAL_HDR64_MAGIC){
        auto dir=nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
        if(dir.VirtualAddress && dir.VirtualAddress<nt->OptionalHeader.SizeOfImage){
            auto imports=reinterpret_cast<IMAGE_IMPORT_DESCRIPTOR*>(base+dir.VirtualAddress);
            for(;imports->Name && !gIdentitySlot;++imports){
                if(!imports->OriginalFirstThunk)continue;
                auto names=reinterpret_cast<IMAGE_THUNK_DATA64*>(base+imports->OriginalFirstThunk);
                auto slots=reinterpret_cast<IMAGE_THUNK_DATA64*>(base+imports->FirstThunk);
                for(;names->u1.AddressOfData;++names,++slots){
                    if(IMAGE_SNAP_BY_ORDINAL64(names->u1.Ordinal))continue;
                    auto name=reinterpret_cast<IMAGE_IMPORT_BY_NAME*>(base+names->u1.AddressOfData);
                    if(strcmp(reinterpret_cast<char*>(name->Name),"IsProcessAnExplorer"))continue;
                    auto slot=reinterpret_cast<void**>(&slots->u1.Function);
                    void* previous=*slot;
                    if(ExchangeGuarded(slot,previous,reinterpret_cast<void*>(&ExplorerIdentityForMenuModule))){
                        gIdentitySlot=slot;gOriginalIdentity=previous;gMenuModule=module;result=S_OK;
                    }else result=LastErrorResult();
                    break;
                }
            }
        }
    }
    if(!gIdentitySlot&&acquired)FreeLibrary(module);
    ReleaseSRWLockExclusive(&gIdentityLock);
    return result;
}
static HRESULT RestoreIdentityCompatibility() {
    AcquireSRWLockExclusive(&gIdentityLock);
    HRESULT result=S_OK;
    if(gIdentitySlot){
        if(ExchangeGuarded(gIdentitySlot,reinterpret_cast<void*>(&ExplorerIdentityForMenuModule),gOriginalIdentity)){
            gIdentitySlot=nullptr;gOriginalIdentity=nullptr;
        }else result=HRESULT_FROM_WIN32(ERROR_BUSY);
    }
    // Release the retained module reference only after all adapter sessions and
    // hook calls drain. Do not unload code while native callbacks remain alive.
    ReleaseSRWLockExclusive(&gIdentityLock);
    return result;
}
static HRESULT EnsureThreadRuntime(Presenter* native){
    if(gThreadRuntime)return gThreadRuntime->status;
    auto owner=new(std::nothrow) RuntimeOwner;if(!owner)return E_OUTOFMEMORY;
    owner->thread=GetCurrentThreadId();
    auto table=*reinterpret_cast<void***>(native);
    if(!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,reinterpret_cast<LPCWSTR>(table[3]),&owner->module)){delete owner;return E_FAIL;}
    HMODULE cohabitation=LoadLibraryExW(L"WinUICohabitation.dll",nullptr,LOAD_LIBRARY_SEARCH_SYSTEM32);
    using Setup=HRESULT(WINAPI*)();using Factory=IUnknown**(__cdecl*)(IUnknown**);using AppInit=void(__cdecl*)();
    auto ensure=cohabitation?reinterpret_cast<Setup>(GetProcAddress(cohabitation,"WinAppSDKCohabitation_EnsureWASDKFrameworkPackageRegistered")):nullptr;
    auto registerThread=cohabitation?reinterpret_cast<Setup>(GetProcAddress(cohabitation,"WinAppSDKCohabitation_RegisterWASDKThread")):nullptr;
    HRESULT status=ensure&&registerThread?ensure():E_NOINTERFACE;
    if(SUCCEEDED(status))status=registerThread();
    if(SUCCEEDED(status)){
        // The caller verified the exact system-module hash before invoking
        // these Microsoft PDB-verified sret factory wrappers.
        auto base=reinterpret_cast<BYTE*>(owner->module);
        try{
            try{reinterpret_cast<Factory>(base+0x86c18)(&owner->controller);}
            catch(...){reinterpret_cast<Factory>(base+0x49ad0)(&owner->queue);if(!owner->queue)throw;}
            reinterpret_cast<AppInit>(base+0x86b90)();
            reinterpret_cast<Factory>(base+0x86d0c)(&owner->manager);
            status=(owner->controller||owner->queue)&&owner->manager?S_OK:E_UNEXPECTED;
        }catch(...){status=E_UNEXPECTED;}
    }
    if(cohabitation)FreeLibrary(cohabitation);
    owner->status=status;
    if(owner->controller||owner->queue||owner->manager){gThreadRuntime=owner;InterlockedIncrement(&gRuntimePins);++gRuntimeInitializations;}
    else{FreeLibrary(owner->module);delete owner;}
    return status;
}
static HRESULT SafeInitialize(Presenter* presenter,IUnknown* callback,HWND owner) {
    __try {return presenter->Initialize(TRUE,callback,owner,2);}
    __except(EXCEPTION_EXECUTE_HANDLER){return E_UNEXPECTED;}
}

using NewItemCallback=HRESULT (WINAPI*)(PCIDLIST_ABSOLUTE,int);
thread_local NewItemCallback gNewItemCallback=nullptr;
extern "C" void zs_menu_set_new_item_callback(NewItemCallback callback) noexcept {gNewItemCallback=callback;}
class Session final : public IServiceProvider, public IOleWindow, public NativeCallback, public IOleCommandTarget, public INewMenuClient {
    LONG refs_=1;
    IShellView* shell_=nullptr;
    NativeCallback* callback_=nullptr;
    Presenter* presenter_=nullptr;

    IServiceProvider* nativeServices_=nullptr;
    IExplorerBrowser* browser_=nullptr;
    IOleCommandTarget* commandTarget_=nullptr;
    HWND window_=nullptr;
    DWORD thread_=0;
    bool retiring_=false;
    void** originalPresenterTable_=nullptr;
    void* presenterTable_[11]{};
    bool standalone_=false;
    bool cancelRequested_=false;
    menu_dispatch::Completion standaloneCompletion_;
    menu_dispatch::Result standaloneResult_;
    std::vector<UINT> standaloneApplications_;
    uint32_t standaloneIntercept_=0;
    menu_dispatch::BeforeInvoke standaloneBefore_=nullptr;
    void* standaloneBeforeContext_=nullptr;
    bool standaloneShift_=false,standaloneControl_=false;
    POINT standalonePoint_{};
    HWND standaloneOwner_=nullptr;

public:
    Session(IExplorerBrowser* browser,IShellView* shell,HWND window,HWND menuOwner=nullptr):shell_(shell),browser_(browser),window_(window),thread_(GetCurrentThreadId()),standalone_(menuOwner!=nullptr),standaloneOwner_(menuOwner){
        shell_->AddRef();browser_->AddRef();
        shell_->QueryInterface(IID_PPV_ARGS(&nativeServices_));
        if(FAILED(shell_->QueryInterface(IID_PPV_ARGS(&commandTarget_))))browser_->QueryInterface(IID_PPV_ARGS(&commandTarget_));

    }
    ~Session(){
        if(callback_)callback_->Release();
        if(commandTarget_)commandTarget_->Release();
        if(nativeServices_)nativeServices_->Release();

        if(browser_)browser_->Release();
        if(shell_)shell_->Release();

    }
    HWND Window()const{return window_;}
    HWND CommandWindow()const{return standaloneOwner_?standaloneOwner_:window_;}
    DWORD Thread()const{return thread_;}
    bool OwnsPresenter(Presenter* presenter)const{return presenter==presenter_&&!retiring_;}
    bool IsOpen()const{return !retiring_&&presenter_&&presenter_->IsContextMenuOpen();}
    bool Cancelled()const{return cancelRequested_;}
    HRESULT Dismiss(){
        if(retiring_||!presenter_)return S_FALSE;
        cancelRequested_=true;
        Microsoft::WRL::ComPtr<Presenter> keep(presenter_);
        keep->DismissContextMenu(L"Request cancelled");return S_OK;
    }
    void ConfigureStandalone(HWND owner,HMENU extra,POINT point,const UINT* applications,UINT applicationCount,
            uint32_t intercept,menu_dispatch::BeforeInvoke before,void* beforeContext){
        standalone_=true;standaloneCompletion_={};standaloneResult_={};standalonePoint_=point;standaloneOwner_=owner;
        standaloneApplications_.clear();if(applicationCount)standaloneApplications_.assign(applications,applications+applicationCount);
        standaloneIntercept_=intercept;standaloneBefore_=before;standaloneBeforeContext_=beforeContext;
        standaloneShift_=GetKeyState(VK_SHIFT)<0;standaloneControl_=GetKeyState(VK_CONTROL)<0;
        AcquireSRWLockExclusive(&gPresenterLock);auto found=gPresenters.find(presenter_);
        if(found!=gPresenters.end()){found->second.extra=extra;found->second.extraPending=extra!=nullptr;}
        ReleaseSRWLockExclusive(&gPresenterLock);
    }
    bool StandaloneDone()const{return standaloneCompletion_.done||retiring_;}
    menu_dispatch::Completion::Wait StandaloneWait(bool ownerAlive,bool quitting,bool open,ULONGLONG now){
        return standaloneCompletion_.Poll(ownerAlive&&!retiring_,quitting||cancelRequested_,open,now);
    }
    menu_dispatch::Result StandaloneResult()const{return standaloneResult_;}
    POINT StandalonePoint()const{return standalonePoint_;}
    bool InstallSelectionBridge(Presenter* native,HMODULE module){
        void** original=*reinterpret_cast<void***>(native);
        constexpr ULONG_PTR known[11]={0x777f0,0xc3c80,0x6dbf0,0xf7300,0x105fc0,0xf4840,0xf4650,0x745a0,0xf8b00,0x7eff0,0xf46f0};
        if(reinterpret_cast<ULONG_PTR>(original)-reinterpret_cast<ULONG_PTR>(module)!=0x194fb0)return false;
        for(size_t i=0;i<11;++i){
            if(reinterpret_cast<ULONG_PTR>(original[i])-reinterpret_cast<ULONG_PTR>(module)!=known[i])return false;
            presenterTable_[i]=original[i];
        }
        auto prepare=reinterpret_cast<NativePrepareFn>(original[4]);
        NativePrepareFn expected=nullptr;
        if(!gNativePrepare.compare_exchange_strong(expected,prepare)&&expected!=prepare)return false;
        presenterTable_[4]=reinterpret_cast<void*>(&PrepareAgile);
        AcquireSRWLockExclusive(&gPresenterLock);
        bool registered=false;
        try{registered=gPresenters.emplace(static_cast<IUnknown*>(native),PresenterRecord{this,thread_,0,gHiddenMenuFeatures}).second;}catch(...){registered=false;}
        ReleaseSRWLockExclusive(&gPresenterLock);
        if(!registered)return false;
        if(InterlockedCompareExchangePointer(reinterpret_cast<void**>(native),presenterTable_,original)!=original){
            AcquireSRWLockExclusive(&gPresenterLock);gPresenters.erase(native);ReleaseSRWLockExclusive(&gPresenterLock);return false;
        }
        originalPresenterTable_=original;return true;
    }
    void BeginSelectionGeneration(){
        AcquireSRWLockExclusive(&gPresenterLock);auto found=gPresenters.find(presenter_);
        if(found!=gPresenters.end()){++found->second.generation;found->second.hidden=gHiddenMenuFeatures;}
        ReleaseSRWLockExclusive(&gPresenterLock);
    }
    void RestoreSelectionBridge(Presenter* native){
        AcquireSRWLockExclusive(&gPresenterLock);gPresenters.erase(native);ReleaseSRWLockExclusive(&gPresenterLock);
        if(originalPresenterTable_&&native){
            InterlockedCompareExchangePointer(reinterpret_cast<void**>(native),originalPresenterTable_,presenterTable_);
            originalPresenterTable_=nullptr;
        }
    }
    bool Matches(IShellView* shell,HWND menuOwner)const{return shell==shell_ && standaloneOwner_==menuOwner && !retiring_;}
    HRESULT CreatePresenter(){
        struct Timing{ULONGLONG start=Micros();~Timing(){gLastPresenterCreateMicros=Micros()-start;}} timing;
        HRESULT hr=shell_->QueryInterface(kCallbackIID,reinterpret_cast<void**>(&callback_));
        if(FAILED(hr))return hr;
        Presenter* candidate=nullptr;
        hr=CoCreateInstance(kPresenterClass,nullptr,CLSCTX_INPROC_SERVER,kPresenterIID,reinterpret_cast<void**>(&candidate));
        if(FAILED(hr))return hr;
        HMODULE module=GetModuleHandleW(L"Windows.UI.FileExplorer.dll");
        const auto table=*reinterpret_cast<void***>(candidate);
        // Fail closed on a changed private ABI. This RVA is verified on this PC.
        if(!module || reinterpret_cast<ULONG_PTR>(table[3])-reinterpret_cast<ULONG_PTR>(module)!=0xf7300){
            candidate->Release();return HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);
        }
        static std::atomic<HMODULE> verified{nullptr};
        wchar_t menuPath[MAX_PATH]{};std::string menuHash;
        if(verified.load()!=module && (!GetModuleFileNameW(module,menuPath,static_cast<DWORD>(std::size(menuPath)))||
           !FileSha256(menuPath,menuHash)||menuHash!=kExpectedMenuSha256)){
            candidate->Release();return HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);
        }
        hr=EnsureIdentityCompatibility();
        if(SUCCEEDED(hr))verified.store(module);
        if(SUCCEEDED(hr))hr=EnsureThreadRuntime(candidate);
        if(SUCCEEDED(hr))hr=menu_command_bridge::InstallVerified(module,&CommandPresenterOwner);
        if(SUCCEEDED(hr))hr=menu_model_filter::InstallVerified(module,&PresenterMenuPolicy,&AppendStandaloneModels);
        if(SUCCEEDED(hr))hr=SafeInitialize(candidate,static_cast<NativeCallback*>(this),standaloneOwner_?standaloneOwner_:GetAncestor(window_,GA_ROOT));
        // The independently owned queue must prevent each presenter from owning
        // a dispatcher whose destructor would shut down the shared UI runtime.
        if(SUCCEEDED(hr)){
            const BYTE* state=reinterpret_cast<const BYTE*>(candidate);
            if(!state[0xb2]||*reinterpret_cast<IUnknown* const*>(state+0x478))hr=E_UNEXPECTED;
        }

        if(SUCCEEDED(hr)&&!InstallSelectionBridge(candidate,module))hr=HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);
        if(SUCCEEDED(hr)){presenter_=candidate;++gPresenterCreations;return S_OK;}
        candidate->Release();return hr;
    }
    HRESULT GetPresenter(REFIID iid,void** out){
        if(retiring_||!presenter_)return E_NOINTERFACE;
        return presenter_->QueryInterface(iid,out);
    }
    void InvokeCommand(UINT command){
        Presenter* native=!retiring_?presenter_:nullptr;
        if(standalone_){standaloneCompletion_.Requested(command);standaloneShift_=GetKeyState(VK_SHIFT)<0;standaloneControl_=GetKeyState(VK_CONTROL)<0;}
        if(native){native->AddRef();native->Invoke(command);native->Release();}
    }
    void Retire(){
        if(retiring_)return;retiring_=true;
        Presenter* previous=presenter_;presenter_=nullptr;
        RestoreSelectionBridge(previous);

        if(previous){
            if(previous->IsContextMenuOpen())previous->DismissContextMenu(L"View closed");
            NativeClosable* closable=nullptr;HRESULT close=previous->QueryInterface(kClosableIID,reinterpret_cast<void**>(&closable));
            if(SUCCEEDED(close)){close=closable->Close();closable->Release();}

            if(SUCCEEDED(close))previous->Release();
            else {gNativeCloseFault=true;InterlockedExchange(&gLastFailure,close);} // Keep at most one failed-to-close presenter per thread.
        }
        NativeCallback* callback=callback_;callback_=nullptr;if(callback)callback->Release();
        IOleCommandTarget* commands=commandTarget_;commandTarget_=nullptr;if(commands)commands->Release();
        IServiceProvider* native=nativeServices_;nativeServices_=nullptr;if(native)native->Release();

        IExplorerBrowser* browser=browser_;browser_=nullptr;if(browser)browser->Release();
        IShellView* shell=shell_;shell_=nullptr;if(shell)shell->Release();
    }
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid,void** out)override{
        if(!out)return E_POINTER;*out=nullptr;
        if(iid==IID_IUnknown||iid==IID_IServiceProvider)*out=static_cast<IServiceProvider*>(this);
        else if(iid==kCallbackIID)*out=static_cast<NativeCallback*>(this);
        else if(iid==IID_IOleWindow)*out=static_cast<IOleWindow*>(this);
        else if(iid==IID_INewMenuClient&&gNewItemCallback)*out=static_cast<INewMenuClient*>(this);
        else if(iid==IID_IOleCommandTarget&&commandTarget_&&!retiring_)*out=static_cast<IOleCommandTarget*>(this);
        else return E_NOINTERFACE;
        AddRef();return S_OK;
    }
    ULONG STDMETHODCALLTYPE AddRef()override{return InterlockedIncrement(&refs_);}
    ULONG STDMETHODCALLTYPE Release()override{ULONG count=InterlockedDecrement(&refs_);if(!count)delete this;return count;}
    HRESULT STDMETHODCALLTYPE GetWindow(HWND* window)override{
        if(!window)return E_POINTER;*window=nullptr;
        if(standalone_){*window=standaloneOwner_;return IsWindow(standaloneOwner_)?S_OK:E_FAIL;}
        return shell_?shell_->GetWindow(window):E_FAIL;
    }
    HRESULT STDMETHODCALLTYPE ContextSensitiveHelp(BOOL)override{return E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE QueryService(REFGUID service,REFIID iid,void** out)override{
        if(!out)return E_POINTER;*out=nullptr;
        if(retiring_||!shell_)return E_NOINTERFACE;
        if(service==SID_SNewMenuClient&&gNewItemCallback)return QueryInterface(iid,out);
        if(service==kPresenterSID)return E_NOINTERFACE; // No recursive presenter construction.
        if(service==SID_SFolderView)return shell_->QueryInterface(iid,out);
        if(service==IID_IOleCommandTarget||((service==SID_STopLevelBrowser||service==SID_SShellBrowser)&&iid==IID_IOleCommandTarget))return QueryInterface(iid,out);
        if(service==SID_STopLevelBrowser||service==SID_SShellBrowser)return browser_->QueryInterface(iid,out);
        return nativeServices_?nativeServices_->QueryService(service,iid,out):E_NOINTERFACE;
    }
    HRESULT STDMETHODCALLTYPE QueryStatus(const GUID* group,ULONG count,OLECMD* commands,OLECMDTEXT* text)override{
        AddRef();IOleCommandTarget* target=!retiring_?commandTarget_:nullptr;if(target)target->AddRef();
        HRESULT hr=target?target->QueryStatus(group,count,commands,text):OLECMDERR_E_NOTSUPPORTED;
        if(target)target->Release();Release();return hr;
    }
    HRESULT STDMETHODCALLTYPE IncludeItems(NMCII_FLAGS* flags)override{
        if(!flags)return E_POINTER;*flags=NMCII_ITEMS|NMCII_FOLDERS;return S_OK;
    }
    HRESULT STDMETHODCALLTYPE SelectAndEditItem(PCIDLIST_ABSOLUTE item,NMCSAEI_FLAGS flags)override{
        return gNewItemCallback?gNewItemCallback(item,flags):E_NOINTERFACE;
    }
    HRESULT STDMETHODCALLTYPE Exec(const GUID* group,DWORD command,DWORD options,VARIANT* input,VARIANT* output)override{
        AddRef();IOleCommandTarget* target=!retiring_?commandTarget_:nullptr;if(target)target->AddRef();
        HRESULT hr=target?target->Exec(group,command,options,input,output):OLECMDERR_E_NOTSUPPORTED;
        if(target)target->Release();Release();return hr;
    }
    template<class Action> void ForwardCallback(Action action,bool allowRetiring=false){
        AddRef();NativeCallback* callback=(!retiring_||allowRetiring)?callback_:nullptr;
        if(callback){callback->AddRef();action(callback);callback->Release();}
        Release();
    }
    void STDMETHODCALLTYPE Invoke(IContextMenu* menu,IShellItemArray* items,HMENU popup,UINT command,POINT point)override{
        if(standalone_){
            AddRef();Microsoft::WRL::ComPtr<IContextMenu> keep(menu);
            if(!standaloneCompletion_.requested){standaloneShift_=GetKeyState(VK_SHIFT)<0;standaloneControl_=GetKeyState(VK_CONTROL)<0;}
            standaloneCompletion_.Requested(command);standalonePoint_=point;
            try{
                standaloneResult_=menu_dispatch::Dispatch(keep.Get(),standaloneOwner_,command,1,point,
                    standaloneShift_,standaloneControl_,standaloneApplications_,standaloneIntercept_,
                    standaloneBefore_,standaloneBeforeContext_);
            }catch(...){standaloneResult_={menu_dispatch::Invoked,command,E_UNEXPECTED,menu_dispatch::Executing};}
            Release();return;
        }
        ForwardCallback([&](NativeCallback* callback){callback->Invoke(menu,items,popup,command,point);});
    }
    void STDMETHODCALLTYPE OnContextMenuDismiss()override{
        if(standalone_){
            if(standaloneResult_.kind==menu_dispatch::Dismissed&&standaloneCompletion_.requested)
                standaloneResult_={menu_dispatch::NativeHandled,standaloneCompletion_.requestedCommand,S_OK,menu_dispatch::Presented};
            standaloneCompletion_.Dismissed();return;
        }
        ForwardCallback([](NativeCallback* callback){callback->OnContextMenuDismiss();},true);
    }
    void STDMETHODCALLTYPE DoExpandedContextMenu(POINT point,int flags,GUID* context,POINT* origin,int kind)override{
        if(standalone_){standaloneResult_={menu_dispatch::Expanded,0,S_OK,menu_dispatch::Presented};standalonePoint_=point;standaloneCompletion_.Dismissed();return;}
        AddRef();
        IShellView* view=!retiring_?shell_:nullptr;if(view)view->AddRef();
        const auto show=gOriginalMenuCallback.load();
        const HRESULT result=show&&view?show(view,point):E_NOTIMPL;
        if(view)view->Release();
        if(FAILED(result)){
            uint32_t hidden=gHiddenMenuFeatures;PresenterMenuPolicy(presenter_,&hidden);
            InterlockedExchange(&gLastFailure,result);
            if(!hidden)ForwardCallback([&](NativeCallback* callback){callback->DoExpandedContextMenu(point,flags,context,origin,kind);});
            else{
                // A failed custom expansion must not reveal disabled entries
                // through the unfiltered system fallback.
                const UINT message=RegisterWindowMessageW(L"ZSDesk.ContextMenuFailure");
                if(message)PostMessageW(GetAncestor(window_,GA_ROOT),message,static_cast<WPARAM>(static_cast<DWORD>(result)),0);
            }
        }
        Release();
    }
    void STDMETHODCALLTYPE SetFocus()override{
        if(standalone_){if(IsWindow(standaloneOwner_))::SetFocus(standaloneOwner_);return;}
        ForwardCallback([](NativeCallback* callback){callback->SetFocus();});
    }
};


// Called on the owning UI thread before asynchronous extension metadata work.
// Keep the public selection apartment-safe and let native Invoke use agile
// references prepared by the guarded worker-side command bridge.
static HRESULT STDMETHODCALLTYPE PrepareAgile(Presenter* native,UINT flags,IUnknown* context,POINT point,IContextMenu* menu,
        UINT first,UINT last,BOOL canRename,const void* verbs,GUID activity,IShellItem* target){
    Session* session=nullptr;
    AcquireSRWLockShared(&gPresenterLock);auto found=gPresenters.find(native);
    if(found!=gPresenters.end()){session=found->second.session;session->AddRef();}
    ReleaseSRWLockShared(&gPresenterLock);
    native->AddRef();ULONGLONG started=Micros();
    const auto original=gNativePrepare.load();
    HRESULT result=original?original(native,flags,context,point,menu,first,last,canRename,verbs,activity,target):E_UNEXPECTED;
    if(SUCCEEDED(result)&&session&&session->Thread()==GetCurrentThreadId()&&session->OwnsPresenter(native)){
        session->BeginSelectionGeneration();
        HRESULT snapshot=menu_selection_agile::DetachVerifiedPresenterSelection(native);
        if(snapshot==S_OK)++gSelectionSnapshots;
        else if(FAILED(snapshot))InterlockedExchange(&gLastFailure,snapshot);
    }
    gLastPrepareMicros=Micros()-started;
    native->Release();if(session)session->Release();return result;
}

namespace {
thread_local Session* active=nullptr;
thread_local bool constructing=false;
thread_local bool closing=false;
thread_local HWND failedWindow=nullptr;
thread_local IShellView* failedView=nullptr;
thread_local HWND failedOwner=nullptr;
thread_local HRESULT failedResult=S_OK;
UINT invoke_message=0;
constexpr UINT_PTR subclass_id=0x5a53444d;
LRESULT CALLBACK menu_proc(HWND hwnd,UINT msg,WPARAM wp,LPARAM lp,UINT_PTR,DWORD_PTR data);
void detach(Session* s) {
    if(!s || active!=s)return;
    active=nullptr;
    RemoveWindowSubclass(s->Window(),menu_proc,subclass_id);
    if(s->CommandWindow()!=s->Window())RemoveWindowSubclass(s->CommandWindow(),menu_proc,subclass_id);
    s->Retire();
    s->Release();
}
LRESULT CALLBACK menu_proc(HWND hwnd,UINT msg,WPARAM wp,LPARAM lp,UINT_PTR,DWORD_PTR data) {
    auto s=reinterpret_cast<Session*>(data);
    if(msg==invoke_message && !lp) {
        ++gCommandMessages;
        s->AddRef(); s->InvokeCommand(static_cast<UINT>(wp)); s->Release(); return 0;
    }
    if(msg==WM_NCDESTROY)detach(s);
    return DefSubclassProc(hwnd,msg,wp,lp);
}
struct Reset { ULONGLONG start=Micros(); ~Reset(){gLastQueryMicros=Micros()-start;constructing=false;} };
}
// Called directly by our own BrowserSite. No controller or cross-process hook.
static HRESULT QueryMenuPresenter(IExplorerBrowser* browser,REFGUID service,REFIID iid,void** out,HWND menuOwner) noexcept {
    if(!out)return E_POINTER;
    *out=nullptr;
    if(!browser || service!=kPresenterSID || iid!=kPresenterIID || constructing || closing || gNativeCloseFault)return E_NOINTERFACE;
    constructing=true; Reset reset;++gRequests;
    if(!invoke_message)invoke_message=RegisterWindowMessageW(L"FILE_EXPLORER_CONTEXTMENU_INVOKEMENUITEM");
    if(!invoke_message)return E_FAIL;
    IShellView* v=nullptr;
    HRESULT hr=browser->GetCurrentView(IID_PPV_ARGS(&v));
    if(FAILED(hr)||!v)return E_NOINTERFACE;
    HWND hwnd=nullptr;
    if(FAILED(v->GetWindow(&hwnd))||!hwnd||GetWindowThreadProcessId(hwnd,nullptr)!=GetCurrentThreadId()){v->Release();return E_NOINTERFACE;}
    if(failedWindow==hwnd&&failedView==v&&failedOwner==menuOwner){v->Release();return failedResult;}
    if(active && (!active->Matches(v,menuOwner)||active->Window()!=hwnd))detach(active);
    if(active){v->Release();return active->GetPresenter(iid,out);}
    failedWindow=nullptr;failedView=nullptr;failedOwner=nullptr;
    IShellView* identity=v;
    auto s=new(std::nothrow) Session(browser,v,hwnd,menuOwner);
    v->Release();if(!s)return E_OUTOFMEMORY;
    try{hr=s->CreatePresenter();}catch(...){hr=E_UNEXPECTED;}
    if(SUCCEEDED(hr) && !closing && IsWindow(hwnd) && SetWindowSubclass(hwnd,menu_proc,subclass_id,reinterpret_cast<DWORD_PTR>(s))){
        // The standalone presenter was initialized with the worker HWND, not
        // the hidden Shell view. Native command-bar clicks are posted to that
        // owner as FILE_EXPLORER_CONTEXTMENU_INVOKEMENUITEM. Route both windows
        // to the same presenter or the popup closes without invoking a command.
        const HWND commandWindow=s->CommandWindow();
        if(commandWindow==hwnd || (IsWindow(commandWindow) && GetWindowThreadProcessId(commandWindow,nullptr)==GetCurrentThreadId() &&
                SetWindowSubclass(commandWindow,menu_proc,subclass_id,reinterpret_cast<DWORD_PTR>(s)))){
            active=s;return s->GetPresenter(iid,out);
        }
        RemoveWindowSubclass(hwnd,menu_proc,subclass_id);
        hr=E_FAIL;
    }
    failedResult=FAILED(hr)?hr:E_NOINTERFACE;failedWindow=hwnd;failedView=identity;failedOwner=menuOwner;
    InterlockedExchange(&gLastFailure,failedResult);
    s->Retire();s->Release();return failedResult;
}
extern "C" HRESULT zs_menu_query(IExplorerBrowser* browser,REFGUID service,REFIID iid,void** out) noexcept {
    return QueryMenuPresenter(browser,service,iid,out,nullptr);
}
extern "C" void zs_menu_shutdown() noexcept {
    closing=true;
    detach(active);
    if(!gSharedHosts.load() && SUCCEEDED(RestoreIdentityCompatibility()) && gMenuModule && !gIdentitySlot){
        FreeLibrary(gMenuModule);gMenuModule=nullptr;
    }
    // This is a view lifecycle operation, not process shutdown. New previews,
    // tab hosts and menu mode changes must still be able to acquire a presenter.
    failedWindow=nullptr;failedView=nullptr;failedOwner=nullptr;failedResult=S_OK;closing=false;
}
extern "C" void zs_menu_shared_host(BOOL enabled) noexcept {
    if(enabled){gSharedHosts.fetch_add(1);}
    else if(gSharedHosts.fetch_sub(1)==1){zs_menu_shutdown();}
}
extern "C" void zs_menu_set_hidden_features(uint32_t hidden) noexcept {
    gHiddenMenuFeatures=hidden;
}
extern "C" void zs_menu_set_original_callback(OriginalMenuCallback callback) noexcept {
    gOriginalMenuCallback.store(callback);
}
// Read-only, thread-local diagnostics used by native integration tests. XAML
// menus do not enter TrackPopupMenu's WM_ENTERMENULOOP, so that message cannot
// be used to prove that the modern presenter is visible or that it received a
// command on the right owner window.
extern "C" BOOL zs_menu_is_open(HWND owner) noexcept {
    return active&&active->CommandWindow()==owner&&active->IsOpen();
}
extern "C" HRESULT zs_menu_dismiss_active(HWND owner) noexcept {
    if(!active||active->CommandWindow()!=owner||active->Thread()!=GetCurrentThreadId())return S_FALSE;
    struct Hold{Session* value;explicit Hold(Session* session):value(session){value->AddRef();}~Hold(){value->Release();}} keep(active);
    try{return keep.value->Dismiss();}catch(...){return E_UNEXPECTED;}
}
extern "C" uint64_t zs_menu_command_messages() noexcept {return gCommandMessages;}
struct ZsMenuDiagnostics {
    uint64_t requests,presenter_creations,selection_snapshots,agile_commands,agile_failures;
    uint64_t last_query_microseconds,last_prepare_microseconds,runtime_initializations;
    int32_t last_failure;uint32_t reserved;
    uint64_t modern_items_hidden,modern_extension_loads_skipped,modern_updates_skipped,modern_filter_failures;
    uint64_t last_presenter_create_microseconds,last_public_bind_microseconds,last_public_populate_microseconds;
};
extern "C" void zs_menu_diagnostics(ZsMenuDiagnostics* out) noexcept {
    if(!out)return;
    *out={gRequests.load(),gPresenterCreations.load(),gSelectionSnapshots.load(),menu_command_bridge::converted.load(),
        menu_command_bridge::failures.load(),gLastQueryMicros,gLastPrepareMicros,gRuntimeInitializations.load(),gLastFailure,0,
        menu_model_filter::removed.load(),menu_model_filter::skippedExtensions.load(),
        menu_model_filter::skippedUpdates.load(),menu_model_filter::failures.load(),gLastPresenterCreateMicros,0,0};
}
#include "StandaloneMenuHost.h"
