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
#include <thread>
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
struct PresenterRecord { Session* session;DWORD thread;ULONGLONG generation;uint32_t hidden;HMENU extra=nullptr;bool extraPending=false;std::weak_ptr<native_command_guard::Gate> gate; };
static SRWLOCK gPresenterLock=SRWLOCK_INIT;
static thread_local bool gProbeExpanded=false;
static std::unordered_map<IUnknown*,PresenterRecord> gPresenters;
static std::atomic<ULONGLONG> gRequests{0},gPresenterCreations{0},gSelectionSnapshots{0},gRuntimeInitializations{0};
static thread_local ULONGLONG gLastQueryMicros=0,gLastPrepareMicros=0,gLastPresenterCreateMicros=0;
static thread_local ULONGLONG gCommandMessages=0;
static thread_local bool gNativeCloseFault=false;
// Test callers can supply a real COM extension without installing an AppX
// package. The same verified native insertion and proxy path is exercised.
static thread_local Microsoft::WRL::ComPtr<IExplorerCommand> gTestExtension;
static thread_local UINT gTestExtensionId=0;
static HRESULT InsertTestExtension(IUnknown* native,HMENU menu){
    if(!gTestExtension)return S_OK;
    auto command=std::move(gTestExtension);
    auto storage=reinterpret_cast<BYTE*>(native)+0x130;
    menu_command_agility::VectorRange range{};size_t beforeCount=0,afterCount=0;
    if(!menu_command_agility::ReadRange(storage,range,beforeCount))return E_UNEXPECTED;
    const std::wstring package;
    menu_command_bridge::Insert(native,menu,command.Get(),GetMenuItemCount(menu),storage,&package);
    if(!menu_command_agility::ReadRange(storage,range,afterCount)||afterCount!=beforeCount+1)return E_UNEXPECTED;
    gTestExtensionId=*reinterpret_cast<UINT*>(range.begin+beforeCount*menu_command_agility::kVerbBytes);
    std::fprintf(stderr,"test-native-extension id=%u\n",gTestExtensionId);
    return gTestExtensionId?S_OK:E_UNEXPECTED;
}
extern "C" void zs_menu_test_extension(IExplorerCommand* command){gTestExtension=command;gTestExtensionId=0;menu_model_filter::testInsertion=&InsertTestExtension;}
extern "C" UINT zs_menu_test_extension_id(){return gTestExtensionId;}
static thread_local uint32_t gHiddenMenuFeatures=menu_model_filter::Wps;
using OriginalMenuCallback=HRESULT(WINAPI*)(IShellView*,POINT);
static std::atomic<OriginalMenuCallback> gOriginalMenuCallback{nullptr};
static ULONGLONG Micros(){LARGE_INTEGER time{},frequency{};QueryPerformanceCounter(&time);QueryPerformanceFrequency(&frequency);const auto ticks=static_cast<ULONGLONG>(time.QuadPart),rate=static_cast<ULONGLONG>(frequency.QuadPart);return (ticks/rate)*1000000+(ticks%rate)*1000000/rate;}
static bool CommandPresenterOwner(IUnknown* native,DWORD* thread,ULONGLONG* generation,std::shared_ptr<native_command_guard::Gate>* gate) noexcept {
    if(!native||!thread||!generation||!gate)return false;gate->reset();
    AcquireSRWLockShared(&gPresenterLock);auto found=gPresenters.find(native);bool valid=found!=gPresenters.end();
    if(valid){*thread=found->second.thread;*generation=found->second.generation;*gate=found->second.gate.lock();valid=static_cast<bool>(*gate);}
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
extern "C" void zs_menu_set_working_directory(PCWSTR path) {menu_dispatch::workingDirectory=path?path:L"";}
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
    std::atomic<bool> retiring_{false};
    bool hasCommandTarget_=false;
    void** originalPresenterTable_=nullptr;
    void* presenterTable_[11]{};
    bool standalone_=false;
    bool cancelRequested_=false;
    menu_dispatch::Completion standaloneCompletion_;
    menu_dispatch::Result standaloneResult_;
    std::vector<UINT> standaloneApplications_;
    uint32_t standaloneIntercept_=0;
    bool standaloneShift_=false,standaloneControl_=false;
    POINT standalonePoint_{};
    POINT standaloneAnchor_{};
    HWND standaloneOwner_=nullptr;
    Microsoft::WRL::ComPtr<IContextMenu> standaloneContext_;
    Microsoft::WRL::ComPtr<IContextMenu> pendingContext_;
    Microsoft::WRL::ComPtr<IShellItemArray> pendingItems_;
    HMENU pendingPopup_=nullptr;
    UINT pendingCommand_=0;
    POINT pendingPoint_{};
    std::shared_ptr<native_command_guard::Gate> commandGate_;
    Microsoft::WRL::ComPtr<IServiceProvider> nativeSite_;

    static HRESULT WINAPI NoInvoke(void*){return E_PENDING;}
    static HRESULT WINAPI ResolveDirect(void* raw,UINT command,menu_dispatch::Result* result,BOOL* handled){
        if(!raw||!result||!handled)return E_POINTER;*handled=FALSE;
        auto self=static_cast<Session*>(raw);self->AddRef();HRESULT hr=S_OK;
        try{
            if(self->retiring_||!self->standaloneContext_)hr=E_ABORT;
            else if(command&&command<=0x6fff){
                Microsoft::WRL::ComPtr<IContextMenu> menu=self->standaloneContext_;
                const auto applications=self->standaloneApplications_;
                const auto probe=menu_dispatch::Dispatch(menu.Get(),self->standaloneOwner_,command,1,self->standalonePoint_,
                    self->standaloneShift_,self->standaloneControl_,applications,self->standaloneIntercept_,&NoInvoke,nullptr);
                if(self->retiring_)hr=E_ABORT;
                else if(probe.kind!=menu_dispatch::Invoked&&probe.kind!=menu_dispatch::NativeHandled){*result=probe;*handled=TRUE;}
                else if(FAILED(probe.status)&&probe.status!=E_PENDING)hr=probe.status;
            }
        }catch(const std::bad_alloc&){hr=E_OUTOFMEMORY;}catch(...){hr=E_UNEXPECTED;}
        self->Release();return hr;
    }
    struct CallbackGrant{std::shared_ptr<native_command_guard::Gate> gate;ULONGLONG generation;UINT command;Microsoft::WRL::ComPtr<IShellItemArray> items;};
    static HRESULT WINAPI BeforeCallback(void* raw){
        const auto value=static_cast<CallbackGrant*>(raw);if(!value||!value->gate)return E_ABORT;
        const auto result=value->gate->Authorize(value->generation,value->command,value->items.Get(),true,true,false);
        return result.execute?S_OK:FAILED(result.status)?result.status:E_ABORT;
    }

public:
    Session(IExplorerBrowser* browser,IShellView* shell,HWND window,HWND menuOwner=nullptr):shell_(shell),browser_(browser),window_(window),thread_(GetCurrentThreadId()),standalone_(menuOwner!=nullptr),standaloneOwner_(menuOwner){
        shell_->AddRef();browser_->AddRef();
        shell_->QueryInterface(IID_PPV_ARGS(&nativeServices_));
        if(FAILED(shell_->QueryInterface(IID_PPV_ARGS(&commandTarget_))))browser_->QueryInterface(IID_PPV_ARGS(&commandTarget_));
        hasCommandTarget_=commandTarget_!=nullptr;

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
        if(commandGate_)commandGate_->Invalidate();
        Microsoft::WRL::ComPtr<Presenter> keep(presenter_);
        keep->DismissContextMenu(L"Request cancelled");return S_OK;
    }
    HRESULT ConfigureStandalone(HWND owner,HMENU extra,POINT point,const UINT* applications,UINT applicationCount,
            uint32_t intercept,menu_dispatch::BeforeInvoke before,void* beforeContext,IContextMenu* context,UINT selectionCount,PCIDLIST_ABSOLUTE const* selection){
        if(commandGate_)commandGate_->Invalidate();nativeSite_.Reset();commandGate_.reset();
        standalone_=true;standaloneCompletion_={};standaloneResult_={};standalonePoint_=point;standaloneAnchor_=point;standaloneOwner_=owner;
        standaloneContext_=context;
        standaloneApplications_.clear();if(applicationCount)standaloneApplications_.assign(applications,applications+applicationCount);
        standaloneIntercept_=intercept;
        standaloneShift_=GetKeyState(VK_SHIFT)<0;standaloneControl_=GetKeyState(VK_CONTROL)<0;
        HRESULT hr=native_command_guard::Gate::Create(owner,before,beforeContext,&ResolveDirect,this,selectionCount,selection,intercept!=0,commandGate_);
        if(FAILED(hr))return hr;
        hr=native_command_guard::Site::Create(static_cast<IServiceProvider*>(this),commandGate_,&nativeSite_);
        if(FAILED(hr)){commandGate_->Invalidate();commandGate_.reset();return hr;}
        AcquireSRWLockExclusive(&gPresenterLock);auto found=gPresenters.find(presenter_);
        if(found!=gPresenters.end()){found->second.extra=extra;found->second.extraPending=extra!=nullptr;found->second.gate=commandGate_;}
        ReleaseSRWLockExclusive(&gPresenterLock);
        return S_OK;
    }
    IServiceProvider* MenuSite()const{return nativeSite_.Get();}
    void ArmCommands(){if(commandGate_)commandGate_->Arm();}
    HRESULT CommandPreparationFailure()const{return commandGate_?commandGate_->State().failure:E_NOINTERFACE;}
    bool StandaloneDone()const{return standaloneCompletion_.done||retiring_;}
    void DispatchPending(){
        // Parent validation and Shell invocation pump messages. Run them only
        // after the native click/dismiss callback has unwound; reentering the
        // presenter's close animation can reset its IContextMenu mid-invoke.
        if(pendingCommand_&&!retiring_&&!cancelRequested_){
            auto menu=std::move(pendingContext_);auto items=std::move(pendingItems_);
            const auto command=pendingCommand_;pendingCommand_=0;
            DispatchCallback(menu.Get(),items.Get(),pendingPopup_,command,pendingPoint_);
        }
    }
    menu_dispatch::Completion::Wait StandaloneWait(bool ownerAlive,bool quitting,bool open,ULONGLONG now){
        if(commandGate_){const auto state=commandGate_->State();
            if(FAILED(state.failure)&&!standaloneCompletion_.requested&&!state.action()){
                std::fprintf(stderr,"native-command-guard fallback before action hr=0x%08X\n",static_cast<unsigned>(state.failure));
                standaloneResult_={menu_dispatch::Expanded,0,S_OK,menu_dispatch::Presented};standaloneCompletion_.Dismissed();
                if(presenter_&&open)presenter_->DismissContextMenu(L"Native command interface unavailable");
            }
            if(state.waiting()&&ownerAlive&&!quitting&&!retiring_&&!cancelRequested_)return menu_dispatch::Completion::Continue;
        }
        return standaloneCompletion_.Poll(ownerAlive&&!retiring_,quitting||cancelRequested_,open,now);
    }
    menu_dispatch::Result StandaloneResult()const{
        if(commandGate_){const auto state=commandGate_->State();if(state.phase==native_command_guard::Phase::Completed||state.phase==native_command_guard::Phase::Denied)return state.outcome;}
        return standaloneResult_;
    }
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
        ULONGLONG generation=0;
        if(found!=gPresenters.end()){generation=++found->second.generation;found->second.hidden=gHiddenMenuFeatures;}
        ReleaseSRWLockExclusive(&gPresenterLock);
        if(commandGate_)commandGate_->Generation(generation);
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
        // Deliver the native click before any parent round trip. Waiting here
        // pumps XAML's closing animation, which can clear the presenter's menu
        // before Invoke reads it. The deferred callback / guarded command owns
        // selection validation and the one-shot authorization before execution.
        Microsoft::WRL::ComPtr<Presenter> native=!retiring_?presenter_:nullptr;
        if(standalone_&&command){
            if(standaloneCompletion_.requested)return;
            if(!commandGate_||!commandGate_->Select(command))return;
            standaloneCompletion_.Requested(command);standaloneShift_=GetKeyState(VK_SHIFT)<0;standaloneControl_=GetKeyState(VK_CONTROL)<0;
        }
        if(native)native->Invoke(command);
    }
    void Retire(){
        if(retiring_)return;retiring_=true;
        pendingCommand_=0;pendingContext_.Reset();pendingItems_.Reset();
        if(commandGate_)commandGate_->Invalidate();nativeSite_.Reset();commandGate_.reset();standaloneContext_.Reset();
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
        else if(iid==IID_IOleCommandTarget&&hasCommandTarget_&&!retiring_)*out=static_cast<IOleCommandTarget*>(this);
        else return E_NOINTERFACE;
        AddRef();return S_OK;
    }
    ULONG STDMETHODCALLTYPE AddRef()override{return InterlockedIncrement(&refs_);}
    ULONG STDMETHODCALLTYPE Release()override{ULONG count=InterlockedDecrement(&refs_);if(!count)delete this;return count;}
    HRESULT STDMETHODCALLTYPE GetWindow(HWND* window)override{
        if(!window)return E_POINTER;*window=nullptr;
        if(GetCurrentThreadId()!=thread_)return RPC_E_WRONG_THREAD;
        if(standalone_){*window=standaloneOwner_;return IsWindow(standaloneOwner_)?S_OK:E_FAIL;}
        return shell_?shell_->GetWindow(window):E_FAIL;
    }
    HRESULT STDMETHODCALLTYPE ContextSensitiveHelp(BOOL)override{return E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE QueryService(REFGUID service,REFIID iid,void** out)override{
        if(!out)return E_POINTER;*out=nullptr;
        if(GetCurrentThreadId()!=thread_)return RPC_E_WRONG_THREAD;
        if(retiring_||!shell_)return E_NOINTERFACE;
        if(service==SID_SNewMenuClient&&gNewItemCallback)return QueryInterface(iid,out);
        if(service==kPresenterSID)return E_NOINTERFACE; // No recursive presenter construction.
        if(service==SID_SFolderView)return shell_->QueryInterface(iid,out);
        if(service==IID_IOleCommandTarget||((service==SID_STopLevelBrowser||service==SID_SShellBrowser)&&iid==IID_IOleCommandTarget))return QueryInterface(iid,out);
        if(service==SID_STopLevelBrowser||service==SID_SShellBrowser)return browser_->QueryInterface(iid,out);
        return nativeServices_?nativeServices_->QueryService(service,iid,out):E_NOINTERFACE;
    }
    HRESULT STDMETHODCALLTYPE QueryStatus(const GUID* group,ULONG count,OLECMD* commands,OLECMDTEXT* text)override{
        if(GetCurrentThreadId()!=thread_)return RPC_E_WRONG_THREAD;
        AddRef();IOleCommandTarget* target=!retiring_?commandTarget_:nullptr;if(target)target->AddRef();
        HRESULT hr=target?target->QueryStatus(group,count,commands,text):OLECMDERR_E_NOTSUPPORTED;
        if(target)target->Release();Release();return hr;
    }
    HRESULT STDMETHODCALLTYPE IncludeItems(NMCII_FLAGS* flags)override{
        if(GetCurrentThreadId()!=thread_)return RPC_E_WRONG_THREAD;
        if(!flags)return E_POINTER;*flags=NMCII_ITEMS|NMCII_FOLDERS;return S_OK;
    }
    HRESULT STDMETHODCALLTYPE SelectAndEditItem(PCIDLIST_ABSOLUTE item,NMCSAEI_FLAGS flags)override{
        if(GetCurrentThreadId()!=thread_)return RPC_E_WRONG_THREAD;
        return gNewItemCallback?gNewItemCallback(item,flags):E_NOINTERFACE;
    }
    HRESULT STDMETHODCALLTYPE Exec(const GUID* group,DWORD command,DWORD options,VARIANT* input,VARIANT* output)override{
        if(GetCurrentThreadId()!=thread_)return RPC_E_WRONG_THREAD;
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
        if(GetCurrentThreadId()!=thread_){++native_command_guard::unobserved;InterlockedExchange(&gLastFailure,RPC_E_WRONG_THREAD);return;}
        if(!standalone_){ForwardCallback([&](NativeCallback* callback){callback->Invoke(menu,items,popup,command,point);});return;}
        if(retiring_||!command||pendingCommand_||commandGate_&&commandGate_->State().action())return;
        pendingContext_=menu;pendingItems_=items;pendingPopup_=popup;pendingCommand_=command;pendingPoint_=point;
        standaloneCompletion_.Requested(command);
    }
    void DispatchCallback(IContextMenu* menu,IShellItemArray* items,HMENU popup,UINT command,POINT point){
        if(GetEnvironmentVariableW(L"ZSDESK_MENU_TEST_PROBE",nullptr,0)){
            wchar_t verb[128]{};HRESULT found=menu?menu->GetCommandString(command-1,GCS_VERBW,nullptr,reinterpret_cast<char*>(verb),128):E_POINTER;
            std::fprintf(stderr,"native-callback command=%u verb=%ls hr=0x%08X\n",command,verb,static_cast<unsigned>(found));
        }
        if(GetCurrentThreadId()!=thread_){++native_command_guard::unobserved;InterlockedExchange(&gLastFailure,RPC_E_WRONG_THREAD);return;}
        if(standalone_){
            AddRef();Microsoft::WRL::ComPtr<IContextMenu> keep(menu);
            if(retiring_){Release();return;}
            // Native dismissal may report Invoke(0) after a terminal expansion
            // or routing decision. It must not erase that already-known result.
            if(!command&&standaloneResult_.kind!=menu_dispatch::Dismissed){Release();return;}
            if(commandGate_&&commandGate_->State().action()){Release();return;}
            if(command>0x6fff&&std::find(standaloneApplications_.begin(),standaloneApplications_.end(),command)==standaloneApplications_.end()){
                standaloneResult_={menu_dispatch::NativeHandled,command,E_NOTIMPL,menu_dispatch::Presented};Release();return;
            }
            if(!standaloneCompletion_.requested){standaloneShift_=GetKeyState(VK_SHIFT)<0;standaloneControl_=GetKeyState(VK_CONTROL)<0;}
            standaloneCompletion_.Requested(command);standalonePoint_=point;
            try{
                CallbackGrant grant{commandGate_,commandGate_?commandGate_->Generation():0,command,{}};grant.items=items;
                const auto applications=standaloneApplications_;
                const auto outcome=menu_dispatch::Dispatch(keep.Get(),standaloneOwner_,command,1,point,
                    standaloneShift_,standaloneControl_,applications,standaloneIntercept_,
                    &BeforeCallback,&grant);
                if(grant.gate){
                    if(grant.gate->State().phase==native_command_guard::Phase::Executing)grant.gate->Complete(grant.generation,outcome.status,menu_dispatch::Invoked);
                    else if(outcome.kind!=menu_dispatch::Invoked&&outcome.kind!=menu_dispatch::Dismissed)grant.gate->Handled(grant.generation,outcome);
                }
                if(!retiring_&&commandGate_==grant.gate&&(!grant.gate||grant.gate->Current(grant.generation)))standaloneResult_=outcome;
            }catch(...){standaloneResult_={menu_dispatch::Invoked,command,E_UNEXPECTED,menu_dispatch::Executing};}
            Release();return;
        }
        ForwardCallback([&](NativeCallback* callback){callback->Invoke(menu,items,popup,command,point);});
    }
    void STDMETHODCALLTYPE OnContextMenuDismiss()override{
        if(GetCurrentThreadId()!=thread_){++native_command_guard::unobserved;InterlockedExchange(&gLastFailure,RPC_E_WRONG_THREAD);return;}
        if(standalone_){
            const auto state=commandGate_?commandGate_->State():native_command_guard::Snapshot{};
            if(!pendingCommand_&&standaloneResult_.kind==menu_dispatch::Dismissed&&standaloneCompletion_.requested&&!state.action()&&state.phase!=native_command_guard::Phase::Denied){
                ++native_command_guard::unobserved;
                standaloneResult_={menu_dispatch::NativeHandled,standaloneCompletion_.requestedCommand,E_NOTIMPL,menu_dispatch::Executing};
            }
            if(commandGate_&&!standaloneCompletion_.requested&&state.phase==native_command_guard::Phase::Fresh)commandGate_->Invalidate();
            standaloneCompletion_.Dismissed();return;
        }
        ForwardCallback([](NativeCallback* callback){callback->OnContextMenuDismiss();},true);
    }
    void STDMETHODCALLTYPE DoExpandedContextMenu(POINT point,int flags,GUID* context,POINT* origin,int kind)override{
        if(GetCurrentThreadId()!=thread_){++native_command_guard::unobserved;InterlockedExchange(&gLastFailure,RPC_E_WRONG_THREAD);return;}
        if(standalone_){
            if(pendingCommand_||(commandGate_&&commandGate_->State().action()))return;
            // The callback point can describe the expanded-menu row rather
            // than the original mouse/keyboard anchor. Both presentations
            // belong to the same immutable screen-space menu request.
            if(gProbeExpanded)std::fprintf(stderr,"expand anchor=%ld,%ld callback=%ld,%ld\n",standaloneAnchor_.x,standaloneAnchor_.y,point.x,point.y);
            standaloneResult_={menu_dispatch::Expanded,0,S_OK,menu_dispatch::Presented};standalonePoint_=standaloneAnchor_;
            // Expansion is a handoff request, not completion of the XAML
            // dismissal. Starting TrackPopupMenu while its popup still owns
            // input causes the classic menu to disappear immediately.
            if(presenter_&&!standaloneCompletion_.done)presenter_->DismissContextMenu(L"Expand context menu");
            if(!presenter_)standaloneCompletion_.Dismissed();return;
        }
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
        if(GetCurrentThreadId()!=thread_){++native_command_guard::unobserved;InterlockedExchange(&gLastFailure,RPC_E_WRONG_THREAD);return;}
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
    if(session&&session->Thread()==GetCurrentThreadId()&&session->OwnsPresenter(native)){
        session->BeginSelectionGeneration();
    }
    HRESULT result=original?original(native,flags,context,point,menu,first,last,canRename,verbs,activity,target):E_UNEXPECTED;
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
    if(!browser || !menuOwner || service!=kPresenterSID || iid!=kPresenterIID || constructing || closing || gNativeCloseFault)return E_NOINTERFACE;
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
extern "C" HRESULT zs_menu_test_request_expanded(HWND owner,POINT point) noexcept {
    if(!active||active->CommandWindow()!=owner||active->Thread()!=GetCurrentThreadId())return E_INVALIDARG;
    gProbeExpanded=true;Session* session=active;session->AddRef();GUID context{};POINT origin=point;session->DoExpandedContextMenu(point,0,&context,&origin,0);
    const auto result=session->StandaloneResult();std::fprintf(stderr,"expand callback: kind=%u status=0x%08X done=%d cancelled=%d\n",result.kind,static_cast<unsigned>(result.status),session->StandaloneDone(),session->Cancelled());session->Release();return S_OK;
}
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

// Referenced only by Rust's native dispatch contract tests; no popup or file
// operation is created here. The supplied fake context counts actual invokes.
extern "C" HRESULT zs_menu_test_dispatch(IContextMenu* context,uint32_t intercept,int shift,
        menu_dispatch::BeforeInvoke before,void* state,menu_dispatch::Result* out) noexcept {
    if(!out)return E_POINTER;
    try{*out=menu_dispatch::Dispatch(context,nullptr,1,1,POINT{},shift!=0,false,{},intercept,before,state);return S_OK;}
    catch(...){*out={menu_dispatch::Invoked,1,E_UNEXPECTED,menu_dispatch::Executing};return E_UNEXPECTED;}
}

namespace native_guard_probe {
using Microsoft::WRL::ComPtr;
struct __declspec(uuid("C9811F72-25D8-4A68-9DB7-A03222A8A1F0")) OptionalMarker:IUnknown{};
struct Before {
    DWORD thread=GetCurrentThreadId();bool permit=true;
    std::atomic<UINT> calls{0};std::atomic<bool> allowed{false};
    std::shared_ptr<native_command_guard::Gate>* retire=nullptr;
};
HRESULT WINAPI Allow(void* raw){
    auto value=static_cast<Before*>(raw);++value->calls;
    if(GetCurrentThreadId()!=value->thread)return RPC_E_WRONG_THREAD;
    if(value->retire&&*value->retire){(*value->retire)->Invalidate();value->retire->reset();}
    value->allowed.store(value->permit);return value->permit?S_OK:E_ACCESSDENIED;
}
class Command final:public IExplorerCommand,public IObjectWithSite,public native_command_guard::PackageNameMetadata,public OptionalMarker{
    std::atomic<ULONG> refs_{1};ComPtr<IUnknown> marshaler_;
public:
    Before* before=nullptr;std::atomic<UINT> calls{0},packageReads{0},stateReads{0};std::atomic<DWORD> invokeThread{0};HRESULT packageResult=S_OK;
    ComPtr<IUnknown> site;ComPtr<IEnumExplorerCommand> children;
    HRESULT Initialize(){return CoCreateFreeThreadedMarshaler(static_cast<IExplorerCommand*>(this),&marshaler_);}
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid,void** output)override{
        if(!output)return E_POINTER;*output=nullptr;
        if(iid==IID_IUnknown||iid==IID_IExplorerCommand||iid==IID_IAgileObject){*output=static_cast<IExplorerCommand*>(this);AddRef();return S_OK;}
        if(iid==IID_IObjectWithSite){*output=static_cast<IObjectWithSite*>(this);AddRef();return S_OK;}
        if(iid==__uuidof(native_command_guard::PackageNameMetadata)){*output=static_cast<native_command_guard::PackageNameMetadata*>(this);AddRef();return S_OK;}
        if(iid==__uuidof(OptionalMarker)){*output=static_cast<OptionalMarker*>(this);AddRef();return S_OK;}
        if(iid==IID_IMarshal)return marshaler_?marshaler_->QueryInterface(iid,output):E_NOINTERFACE;return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef()override{return ++refs_;}
    ULONG STDMETHODCALLTYPE Release()override{ULONG count=--refs_;if(!count)delete this;return count;}
    HRESULT STDMETHODCALLTYPE GetTitle(IShellItemArray*,LPWSTR* value)override{if(value)*value=nullptr;return E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE GetIcon(IShellItemArray*,LPWSTR* value)override{if(value)*value=nullptr;return E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE GetToolTip(IShellItemArray*,LPWSTR* value)override{if(value)*value=nullptr;return E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE GetCanonicalName(GUID* value)override{if(!value)return E_POINTER;*value=GUID{};return S_OK;}
    HRESULT STDMETHODCALLTYPE GetState(IShellItemArray*,BOOL,EXPCMDSTATE* value)override{
        if(!value)return E_POINTER;*value=ECS_DISABLED;++stateReads;
        if(site){ComPtr<IServiceProvider> provider,service;HRESULT hr=GetSite(IID_PPV_ARGS(&provider));
            if(SUCCEEDED(hr))hr=provider->QueryService(IID_IServiceProvider,IID_PPV_ARGS(&service));if(FAILED(hr))return hr;}
        *value=ECS_ENABLED;return S_OK;
    }
    HRESULT STDMETHODCALLTYPE Invoke(IShellItemArray*,IBindCtx*)override{++calls;invokeThread=GetCurrentThreadId();return before&&before->allowed.load()?S_OK:E_UNEXPECTED;}
    HRESULT STDMETHODCALLTYPE GetFlags(EXPCMDFLAGS* value)override{if(!value)return E_POINTER;*value=ECF_DEFAULT;return S_OK;}
    HRESULT STDMETHODCALLTYPE GetPackageFullName(IShellItemArray*,PWSTR* value)override{
        if(!value)return E_POINTER;*value=nullptr;++packageReads;
        if(FAILED(packageResult))return packageResult;
        constexpr wchar_t package[]=L"ZSDesk.MockPackage_1.0.0.0_x64__test";
        auto allocated=static_cast<PWSTR>(CoTaskMemAlloc(sizeof(package)));if(!allocated)return E_OUTOFMEMORY;
        std::memcpy(allocated,package,sizeof(package));*value=allocated;return S_OK;
    }
    HRESULT STDMETHODCALLTYPE EnumSubCommands(IEnumExplorerCommand** value)override{if(!value)return E_POINTER;*value=nullptr;return children?children->Clone(value):E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE SetSite(IUnknown* value)override{site=value;return S_OK;}
    HRESULT STDMETHODCALLTYPE GetSite(REFIID iid,void** value)override{if(!value)return E_POINTER;*value=nullptr;return site?site->QueryInterface(iid,value):E_FAIL;}
};
HRESULT Make(HWND owner,Before& before,std::shared_ptr<native_command_guard::Gate>& gate){
    HRESULT hr=native_command_guard::Gate::Create(owner,&Allow,&before,nullptr,nullptr,0,nullptr,true,gate);
    if(SUCCEEDED(hr)){gate->Generation(1);gate->Arm();}return hr;
}
HRESULT Wrap(Before& before,const std::shared_ptr<native_command_guard::Gate>& gate,UINT id,ComPtr<Command>& original,ComPtr<IExplorerCommand>& proxy){
    original.Attach(new(std::nothrow) Command);if(!original)return E_OUTOFMEMORY;original->before=&before;
    HRESULT hr=original->Initialize();if(SUCCEEDED(hr))hr=native_command_guard::WrapCommand(original.Get(),gate,1,{},&proxy);
    ComPtr<native_command_guard::ProxyIdentity> identity;
    if(SUCCEEDED(hr))hr=proxy->QueryInterface(native_command_guard::kProxyIdentity,reinterpret_cast<void**>(identity.GetAddressOf()));
    return FAILED(hr)?hr:identity->BindCommand(id);
}
HRESULT Run(uint32_t& passed){
    using native_command_guard::Gate;using native_command_guard::Phase;
    HWND owner=CreateWindowExW(0,L"STATIC",L"",0,0,0,0,0,HWND_MESSAGE,nullptr,GetModuleHandleW(nullptr),nullptr);if(!owner)return HRESULT_FROM_WIN32(GetLastError());
    struct Window{HWND value;~Window(){DestroyWindow(value);}} window{owner};
    {
        Before before;std::shared_ptr<Gate> gate;HRESULT hr=Make(owner,before,gate);if(FAILED(hr))return hr;
        ComPtr<Command> first,other;ComPtr<IExplorerCommand> firstProxy,otherProxy;
        hr=Wrap(before,gate,1,first,firstProxy);if(SUCCEEDED(hr))hr=Wrap(before,gate,2,other,otherProxy);if(FAILED(hr))return hr;
        ComPtr<native_command_guard::ProxyIdentity> alias;
        if(FAILED(firstProxy->QueryInterface(native_command_guard::kProxyIdentity,reinterpret_cast<void**>(alias.GetAddressOf())))||FAILED(alias->BindCommand(3))||!gate->Select(1)||gate->Select(2)
                ||gate->State().phase!=Phase::Fresh||before.calls.load()!=0)return E_UNEXPECTED;
        ComPtr<IUnknown> unsupported;
        if(firstProxy->QueryInterface(__uuidof(OptionalMarker),reinterpret_cast<void**>(unsupported.GetAddressOf()))!=E_NOINTERFACE||unsupported||FAILED(gate->State().failure)||before.calls.load()!=0||first->calls.load()!=0)return E_UNEXPECTED;
        ComPtr<native_command_guard::PackageNameMetadata> package;
        hr=firstProxy.As(&package);if(FAILED(hr))return hr;
        PWSTR name=nullptr;hr=package->GetPackageFullName(nullptr,&name);
        const bool exact=SUCCEEDED(hr)&&name&&std::wcscmp(name,L"ZSDesk.MockPackage_1.0.0.0_x64__test")==0;
        if(name)CoTaskMemFree(name);
        if(!exact||before.calls.load()!=0||first->calls.load()!=0||first->packageReads.load()!=1||FAILED(gate->State().failure))return E_UNEXPECTED;
        first->packageResult=E_NOTIMPL;name=nullptr;
        if(package->GetPackageFullName(nullptr,&name)!=E_NOTIMPL||name||FAILED(gate->State().failure)||before.calls.load()!=0)return E_UNEXPECTED;
        first->packageResult=S_OK;
        ComPtr<IUnknown> packageIdentity,commandIdentity;package.As(&packageIdentity);firstProxy.As(&commandIdentity);
        if(packageIdentity!=commandIdentity)return E_UNEXPECTED;
        auto grant=gate->Authorize(1,1,nullptr,false,false,false);EXPCMDFLAGS flags=ECF_DEFAULT;
        if(FAILED(grant.status)||FAILED(firstProxy->GetFlags(&flags)))return E_UNEXPECTED;
        ComPtr<IEnumExplorerCommand> noChildren;
        if(firstProxy->EnumSubCommands(&noChildren)!=E_NOTIMPL||FAILED(gate->State().failure))return E_UNEXPECTED;
        if(SUCCEEDED(otherProxy->Invoke(nullptr,nullptr))||other->calls.load()!=0)return E_UNEXPECTED;
        if(FAILED(firstProxy->Invoke(nullptr,nullptr))||first->calls.load()!=1||before.calls.load()!=1)return E_UNEXPECTED;
        if(SUCCEEDED(firstProxy->Invoke(nullptr,nullptr))||first->calls.load()!=1)return E_UNEXPECTED;passed|=1;
        gate->Invalidate();
    }
    {
        Before before;before.permit=false;std::shared_ptr<Gate> gate;HRESULT hr=Make(owner,before,gate);if(FAILED(hr))return hr;
        ComPtr<Command> original;ComPtr<IExplorerCommand> proxy;hr=Wrap(before,gate,1,original,proxy);if(FAILED(hr))return hr;
        if(proxy->Invoke(nullptr,nullptr)!=E_ACCESSDENIED||original->calls.load()!=0||before.calls.load()!=1)return E_UNEXPECTED;passed|=2;gate->Invalidate();
    }
    {
        Before before;std::shared_ptr<Gate> gate;HRESULT hr=Make(owner,before,gate);if(FAILED(hr))return hr;
        if(!gate->Authorize(1,1,nullptr,false,true,false).execute)return E_UNEXPECTED;
        gate->Generation(2);gate->Arm();if(!gate->Authorize(2,2,nullptr,false,true,false).execute)return E_UNEXPECTED;
        gate->Complete(1,E_ACCESSDENIED);auto state=gate->State();if(state.phase!=Phase::Executing||state.command!=2)return E_UNEXPECTED;
        gate->Complete(2,S_OK);state=gate->State();if(state.phase!=Phase::Completed||state.command!=2||state.outcome.status!=S_OK)return E_UNEXPECTED;passed|=4;gate->Invalidate();
    }
    {
        Before before;std::shared_ptr<Gate> gate;HRESULT hr=Make(owner,before,gate);if(FAILED(hr))return hr;
        ComPtr<Command> original;ComPtr<IExplorerCommand> proxy;hr=Wrap(before,gate,1,original,proxy);if(FAILED(hr))return hr;
        std::weak_ptr<Gate> weak=gate;before.retire=&gate;
        if(SUCCEEDED(proxy->Invoke(nullptr,nullptr))||original->calls.load()!=0||before.calls.load()!=1||!weak.expired())return E_UNEXPECTED;passed|=8;
    }
    return S_OK;
}
}
namespace native_guard_probe {
struct EnumCounts {std::atomic<UINT> next{0},skip{0},reset{0},clone{0};};
class Enumerator final:public IEnumExplorerCommand{
    std::atomic<ULONG> refs_{1};ComPtr<IUnknown> marshaler_;
    std::vector<ComPtr<IExplorerCommand>> values_;ULONG position_=0;std::shared_ptr<EnumCounts> counts_;
public:
    static HRESULT Create(const std::vector<ComPtr<IExplorerCommand>>& values,ULONG position,const std::shared_ptr<EnumCounts>& counts,IEnumExplorerCommand** output){
        if(!output)return E_POINTER;*output=nullptr;ComPtr<Enumerator> value;value.Attach(new(std::nothrow) Enumerator);if(!value)return E_OUTOFMEMORY;
        value->values_=values;value->position_=position;value->counts_=counts;
        HRESULT hr=CoCreateFreeThreadedMarshaler(static_cast<IEnumExplorerCommand*>(value.Get()),&value->marshaler_);
        if(FAILED(hr))return hr;*output=value.Detach();return S_OK;
    }
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid,void** output)override{
        if(!output)return E_POINTER;*output=nullptr;
        if(iid==IID_IUnknown||iid==IID_IEnumExplorerCommand||iid==IID_IAgileObject){*output=static_cast<IEnumExplorerCommand*>(this);AddRef();return S_OK;}
        return iid==IID_IMarshal?marshaler_->QueryInterface(iid,output):E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef()override{return ++refs_;}
    ULONG STDMETHODCALLTYPE Release()override{ULONG count=--refs_;if(!count)delete this;return count;}
    HRESULT STDMETHODCALLTYPE Next(ULONG count,IExplorerCommand** output,ULONG* fetched)override{
        if(fetched)*fetched=0;if(!output||(!fetched&&count!=1))return E_INVALIDARG;++counts_->next;
        ULONG received=0;for(ULONG index=0;index<count;++index)output[index]=nullptr;
        while(received<count&&position_<values_.size()){output[received]=values_[position_++].Get();output[received++]->AddRef();}
        if(fetched)*fetched=received;return received==count?S_OK:S_FALSE;
    }
    HRESULT STDMETHODCALLTYPE Skip(ULONG count)override{++counts_->skip;ULONG available=static_cast<ULONG>(values_.size())-position_;position_+=(std::min)(count,available);return count<=available?S_OK:S_FALSE;}
    HRESULT STDMETHODCALLTYPE Reset()override{++counts_->reset;position_=0;return S_OK;}
    HRESULT STDMETHODCALLTYPE Clone(IEnumExplorerCommand** output)override{++counts_->clone;return Create(values_,position_,counts_,output);}
};
class StaSite final:public IServiceProvider{
    std::atomic<ULONG> refs_{1};DWORD thread_=GetCurrentThreadId();
public:
    std::atomic<UINT> calls{0},foreign{0};
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid,void** output)override{
        if(!output)return E_POINTER;*output=nullptr;
        if(iid!=IID_IUnknown&&iid!=IID_IServiceProvider)return E_NOINTERFACE;
        *output=static_cast<IServiceProvider*>(this);AddRef();return S_OK;
    }
    ULONG STDMETHODCALLTYPE AddRef()override{return ++refs_;}
    ULONG STDMETHODCALLTYPE Release()override{ULONG count=--refs_;if(!count)delete this;return count;}
    HRESULT STDMETHODCALLTYPE QueryService(REFGUID service,REFIID iid,void** output)override{
        if(!output)return E_POINTER;*output=nullptr;++calls;
        if(GetCurrentThreadId()!=thread_){++foreign;return RPC_E_WRONG_THREAD;}
        return service==IID_IServiceProvider?QueryInterface(iid,output):E_NOINTERFACE;
    }
};
HRESULT Bind(IExplorerCommand* command,UINT id){
    ComPtr<native_command_guard::ProxyIdentity> identity;
    HRESULT hr=command->QueryInterface(native_command_guard::kProxyIdentity,reinterpret_cast<void**>(identity.GetAddressOf()));
    return FAILED(hr)?hr:identity->BindCommand(id);
}
HRESULT OnMta(IExplorerCommand* command,const std::shared_ptr<native_command_guard::Gate>& gate,bool invoke){
    ComPtr<IAgileReference> reference;HRESULT hr=RoGetAgileReference(AGILEREFERENCE_DEFAULT,IID_IExplorerCommand,command,&reference);if(FAILED(hr))return hr;
    std::atomic<bool> done{false};HRESULT result=E_UNEXPECTED;
    std::thread worker([reference,invoke,&done,&result]()mutable{
        const HRESULT initialized=CoInitializeEx(nullptr,COINIT_MULTITHREADED);
        {
            ComPtr<IAgileReference> local;local.Swap(reference);ComPtr<IExplorerCommand> resolved;
            result=FAILED(initialized)?initialized:local->Resolve(IID_PPV_ARGS(&resolved));
            if(SUCCEEDED(result)){EXPCMDSTATE state=ECS_DISABLED;result=invoke?resolved->Invoke(nullptr,nullptr):resolved->GetState(nullptr,TRUE,&state);
                if(!invoke&&SUCCEEDED(result)&&state!=ECS_ENABLED)result=E_UNEXPECTED;}
        }
        if(SUCCEEDED(initialized))CoUninitialize();done.store(true);
    });
    const ULONGLONG deadline=GetTickCount64()+10000;bool expired=false,quit=false;WPARAM quitCode=0;
    while(!done.load()){
        MsgWaitForMultipleObjectsEx(0,nullptr,25,QS_ALLINPUT,MWMO_INPUTAVAILABLE);
        MSG message;while(PeekMessageW(&message,nullptr,0,0,PM_REMOVE)){
            if(message.message==WM_QUIT){quit=true;quitCode=message.wParam;}else{TranslateMessage(&message);DispatchMessageW(&message);}
        }
        if(!expired&&GetTickCount64()>=deadline){expired=true;gate->Invalidate();}
    }
    worker.join();if(quit)PostQuitMessage(static_cast<int>(quitCode));
    return expired?HRESULT_FROM_WIN32(ERROR_TIMEOUT):result;
}
HRESULT WINAPI CopyAction(void* context,UINT command,menu_dispatch::Result* result,BOOL* handled){
    if(!result||!handled)return E_POINTER;auto before=static_cast<Before*>(context);
    if(GetCurrentThreadId()!=before->thread)return RPC_E_WRONG_THREAD;
    *handled=command==7;*result={menu_dispatch::Copy,command,S_OK,menu_dispatch::Presented};return S_OK;
}
HRESULT RunAsync(uint32_t& passed){
    using native_command_guard::Gate;using native_command_guard::kCancelled;
    HWND owner=CreateWindowExW(0,L"STATIC",L"",0,0,0,0,0,HWND_MESSAGE,nullptr,GetModuleHandleW(nullptr),nullptr);if(!owner)return HRESULT_FROM_WIN32(GetLastError());
    struct Window{HWND value;~Window(){DestroyWindow(value);}} window{owner};
    {
        Before before;std::shared_ptr<Gate> gate;HRESULT hr=Make(owner,before,gate);if(FAILED(hr))return hr;
        ComPtr<Command> original;ComPtr<IExplorerCommand> proxy;hr=Wrap(before,gate,1,original,proxy);if(FAILED(hr))return hr;
        hr=OnMta(proxy.Get(),gate,true);if(FAILED(hr))return hr;
        if(before.calls.load()!=1||original->calls.load()!=1||original->invokeThread.load()==before.thread||SUCCEEDED(OnMta(proxy.Get(),gate,true))||original->calls.load()!=1)return E_UNEXPECTED;
        passed|=1;gate->Invalidate();
    }
    {
        Before before;std::shared_ptr<Gate> gate;HRESULT hr=Make(owner,before,gate);if(FAILED(hr))return hr;
        ComPtr<Command> original;ComPtr<IExplorerCommand> proxy;hr=Wrap(before,gate,1,original,proxy);if(FAILED(hr))return hr;
        ComPtr<StaSite> raw;raw.Attach(new(std::nothrow) StaSite);if(!raw)return E_OUTOFMEMORY;
        // The negative control invokes only a mock thread-checking method.
        original->SetSite(raw.Get());hr=OnMta(proxy.Get(),gate,false);
        if(hr!=RPC_E_WRONG_THREAD||raw->foreign.load()!=1||raw->calls.load()!=1)return E_UNEXPECTED;
        ComPtr<IServiceProvider> bridge;hr=native_command_guard::Site::Create(raw.Get(),gate,&bridge);if(FAILED(hr))return hr;
        ComPtr<IObjectWithSite> withSite;hr=proxy.As(&withSite);if(SUCCEEDED(hr))hr=withSite->SetSite(bridge.Get());if(FAILED(hr))return hr;
        hr=OnMta(proxy.Get(),gate,false);if(FAILED(hr))return hr;
        if(raw->calls.load()!=2||raw->foreign.load()!=1||original->stateReads.load()!=2||before.calls.load()!=0||original->calls.load()!=0)return E_UNEXPECTED;
        gate->Invalidate();ComPtr<IServiceProvider> late;
        if(bridge->QueryService(IID_IServiceProvider,IID_PPV_ARGS(&late))!=kCancelled||late||raw->calls.load()!=2)return E_UNEXPECTED;
        original->SetSite(nullptr);passed|=2;
    }
    {
        Before before;std::shared_ptr<Gate> gate;HRESULT hr=Make(owner,before,gate);if(FAILED(hr))return hr;
        ComPtr<Command> parent,child;ComPtr<IExplorerCommand> parentProxy,unused;
        hr=Wrap(before,gate,1,parent,parentProxy);if(SUCCEEDED(hr))hr=Wrap(before,gate,2,child,unused);if(FAILED(hr))return hr;unused.Reset();
        auto counts=std::make_shared<EnumCounts>();std::vector<ComPtr<IExplorerCommand>> values{child.Get()};
        hr=Enumerator::Create(values,0,counts,&parent->children);if(FAILED(hr))return hr;
        ComPtr<IEnumExplorerCommand> enumerator,clone;
        hr=parentProxy->EnumSubCommands(&enumerator);if(SUCCEEDED(hr))hr=enumerator->Clone(&clone);if(FAILED(hr))return hr;
        ComPtr<IExplorerCommand> first,second;ULONG fetched=0;
        hr=enumerator->Next(1,&first,&fetched);if(hr!=S_OK||fetched!=1||!first)return E_UNEXPECTED;
        hr=clone->Next(1,&second,&fetched);if(hr!=S_OK||fetched!=1||!second)return E_UNEXPECTED;
        if(FAILED(Bind(first.Get(),2))||FAILED(Bind(second.Get(),2))||FAILED(first->Invoke(nullptr,nullptr))||SUCCEEDED(second->Invoke(nullptr,nullptr))||child->calls.load()!=1||before.calls.load()!=1)return E_UNEXPECTED;
        if(enumerator->Next(1,unused.ReleaseAndGetAddressOf(),&fetched)!=S_FALSE||fetched||unused)return E_UNEXPECTED;
        if(FAILED(clone->Reset())||clone->Skip(1)!=S_OK||clone->Next(1,unused.ReleaseAndGetAddressOf(),&fetched)!=S_FALSE)return E_UNEXPECTED;
        gate->Generation(2);gate->Arm();const UINT next=counts->next.load(),skip=counts->skip.load(),reset=counts->reset.load(),clones=counts->clone.load();
        ComPtr<IEnumExplorerCommand> staleClone;
        if(enumerator->Next(1,unused.ReleaseAndGetAddressOf(),&fetched)!=kCancelled||fetched||unused||clone->Skip(1)!=kCancelled||clone->Reset()!=kCancelled||clone->Clone(&staleClone)!=kCancelled||staleClone||first->Invoke(nullptr,nullptr)!=kCancelled)return E_UNEXPECTED;
        if(counts->next.load()!=next||counts->skip.load()!=skip||counts->reset.load()!=reset||counts->clone.load()!=clones||child->calls.load()!=1||before.calls.load()!=1)return E_UNEXPECTED;
        passed|=4;gate->Invalidate();
    }
    {
        for(bool wrapped:{false,true}){
            Before before;std::shared_ptr<Gate> gate;HRESULT hr=Make(owner,before,gate);if(FAILED(hr))return hr;
            ComPtr<Command> parent;ComPtr<IExplorerCommand> proxy;hr=Wrap(before,gate,1,parent,proxy);if(FAILED(hr))return hr;
            struct ClearChildren{Command* value;~ClearChildren(){value->children.Reset();}} clear{parent.Get()};
            auto counts=std::make_shared<EnumCounts>();std::vector<ComPtr<IExplorerCommand>> values{wrapped?proxy.Get():static_cast<IExplorerCommand*>(parent.Get())};
            hr=Enumerator::Create(values,0,counts,&parent->children);if(FAILED(hr))return hr;
            ComPtr<IEnumExplorerCommand> enumerator;hr=proxy->EnumSubCommands(&enumerator);if(FAILED(hr))return hr;
            ComPtr<IExplorerCommand> child;ULONG fetched=99;hr=enumerator->Next(1,&child,&fetched);
            if(hr!=HRESULT_FROM_WIN32(ERROR_CIRCULAR_DEPENDENCY)||child||fetched||before.calls.load()!=0||parent->calls.load()!=0)return E_UNEXPECTED;
            gate->Invalidate();
        }
        passed|=8;
    }
    {
        Before before;std::shared_ptr<Gate> gate;
        HRESULT hr=Gate::Create(owner,&Allow,&before,&CopyAction,&before,0,nullptr,true,gate);if(FAILED(hr))return hr;gate->Generation(1);gate->Arm();
        ComPtr<Command> original;ComPtr<IExplorerCommand> proxy;hr=Wrap(before,gate,7,original,proxy);if(FAILED(hr))return hr;
        hr=OnMta(proxy.Get(),gate,true);if(FAILED(hr))return hr;const auto state=gate->State();
        if(state.phase!=native_command_guard::Phase::Completed||state.outcome.kind!=menu_dispatch::Copy||state.outcome.command!=7||before.calls.load()!=0||original->calls.load()!=0||SUCCEEDED(proxy->Invoke(nullptr,nullptr)))return E_UNEXPECTED;
        passed|=16;gate->Invalidate();
    }
    return S_OK;
}
}
// Pure counter/marshal tests: hidden message windows only, no Shell extension,
// registry, clipboard, file command or visible menu is invoked. Bitmask 0xf:
// cross-ID/duplicate, denied-zero-call, generation completion, reentrant retire.
extern "C" HRESULT zs_menu_test_command_guard(uint32_t* passed) noexcept {
    if(!passed)return E_POINTER;*passed=0;HRESULT initialized=OleInitialize(nullptr);if(FAILED(initialized))return initialized;
    HRESULT result=E_UNEXPECTED;try{result=native_guard_probe::Run(*passed);}catch(const std::bad_alloc&){result=E_OUTOFMEMORY;}catch(...){result=E_UNEXPECTED;}
    OleUninitialize();return result;
}
// Bitmask 0x1f: MTA invoke, STA site, child enumeration/generation, cycle
// rejection, parent-owned action. All commands and services are local mocks.
extern "C" HRESULT zs_menu_test_command_guard_async(uint32_t* passed) noexcept {
    if(!passed)return E_POINTER;*passed=0;HRESULT initialized=OleInitialize(nullptr);if(FAILED(initialized))return initialized;
    HRESULT result=E_UNEXPECTED;try{result=native_guard_probe::RunAsync(*passed);}catch(const std::bad_alloc&){result=E_OUTOFMEMORY;}catch(...){result=E_UNEXPECTED;}
    OleUninitialize();return result;
}
struct ZsCommandGuardDiagnostics{uint64_t authorized,denied,invoked,wrapped,unsupported,unobserved;};
extern "C" void zs_menu_command_guard_diagnostics(ZsCommandGuardDiagnostics* out) noexcept{
    if(out)*out={native_command_guard::authorized.load(),native_command_guard::denied.load(),native_command_guard::invoked.load(),native_command_guard::wrapped.load(),native_command_guard::unsupported.load(),native_command_guard::unobserved.load()};
}
