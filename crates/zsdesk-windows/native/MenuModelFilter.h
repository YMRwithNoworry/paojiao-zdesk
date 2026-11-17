#pragma once
#include <windows.h>
#include <inspectable.h>
#include <winstring.h>
#include <wrl/client.h>
#include <atomic>
#include <cstdint>
#include <cstring>
#include <string_view>
#include "MinHook.h"
#include "CommandAgility.h"

// Windows.UI.FileExplorer.dll private ABI. InstallVerified is called only after
// the full image digest and presenter vtable have matched the supported build.
namespace menu_model_filter {
enum Hidden : uint32_t {
    Cut=1u<<0, Copy=1u<<1, Paste=1u<<2, PasteLink=1u<<3,
    CopyPath=1u<<4, Rename=1u<<5, Delete=1u<<6, Properties=1u<<7,
    Share=1u<<8, New=1u<<9, SevenZip=1u<<10, Wps=1u<<11,
};
inline constexpr uint32_t builtinMask=(1u<<10)-1;
using PolicyProbe=bool(*)(IUnknown*,uint32_t*) noexcept;
using ModelAugmenter=HRESULT(*)(IUnknown*,IUnknown**) noexcept;
using CreateFunction=IUnknown**(__fastcall*)(IUnknown*,IUnknown**,HMENU);
using ExtensionFunction=void(__fastcall*)(IUnknown*,HMENU,const GUID*,const HSTRING*);
using UpdateFunction=bool(__fastcall*)(IUnknown*,const HSTRING*,const MENUITEMINFOW&,ULONG);
inline CreateFunction originalCreate=nullptr;
inline ExtensionFunction originalExtension=nullptr;
inline UpdateFunction originalUpdate=nullptr;
inline PolicyProbe policyProbe=nullptr;
inline ModelAugmenter modelAugmenter=nullptr;
inline HRESULT(*testInsertion)(IUnknown*,HMENU)=nullptr;
inline thread_local unsigned createDepth=0;
inline thread_local HMENU expectedProjectionRoot=nullptr;
inline thread_local bool expectedProjectionObserved=false;
struct ProjectionProbe{
    HMENU previous;bool previousObserved,enabled;
    ProjectionProbe(HMENU menu,bool active):previous(expectedProjectionRoot),previousObserved(expectedProjectionObserved),enabled(active){
        if(enabled){expectedProjectionRoot=menu;expectedProjectionObserved=false;}
    }
    bool Observed()const{return enabled&&expectedProjectionObserved;}
    ~ProjectionProbe(){if(enabled){expectedProjectionRoot=previous;expectedProjectionObserved=previousObserved;}}
};
inline SRWLOCK setupLock=SRWLOCK_INIT;
inline HMODULE retainedModule=nullptr;
inline HRESULT setupFailure=S_OK;
inline std::atomic<bool> enabled{false};
inline std::atomic<uint64_t> removed{0},skippedExtensions{0},skippedUpdates{0},failures{0};

// The verified system function returns IIterable<MenuItem> backed by its own
// IVector<MenuItem>. Removing from that vector changes the actual XAML model,
// including its command bar, without changing native command IDs or handlers.
struct ItemVector:IInspectable {
    virtual HRESULT STDMETHODCALLTYPE GetAt(UINT,IInspectable**)=0;
    virtual HRESULT STDMETHODCALLTYPE Size(UINT*)=0;
    virtual HRESULT STDMETHODCALLTYPE GetView(IInspectable**)=0;
    virtual HRESULT STDMETHODCALLTYPE IndexOf(IInspectable*,UINT*,boolean*)=0;
    virtual HRESULT STDMETHODCALLTYPE SetAt(UINT,IInspectable*)=0;
    virtual HRESULT STDMETHODCALLTYPE InsertAt(UINT,IInspectable*)=0;
    virtual HRESULT STDMETHODCALLTYPE RemoveAt(UINT)=0;
};
inline constexpr GUID itemVectorIid={0x173a4db8,0x3071,0x5084,{0x88,0x36,0x55,0x24,0x5a,0x88,0x8c,0x95}};
inline constexpr GUID sevenZipModern={0xd8f7c3cd,0x8326,0x4726,{0x9f,0xba,0x1d,0x0a,0x87,0xdb,0x02,0x6f}};
inline constexpr GUID sevenZipVendor={0x23170f69,0x40c1,0x278a,{0x10,0x00,0x00,0x01,0x00,0x02,0x00,0x00}};
inline constexpr GUID wpsExtension={0xeae7d0a0,0x4863,0x4da0,{0x85,0x2c,0x34,0x2b,0x00,0xf6,0x84,0x90}};
inline bool Equal(std::wstring_view id,std::wstring_view known) noexcept {
    return id.size()==known.size()&&CompareStringOrdinal(id.data(),static_cast<int>(id.size()),known.data(),static_cast<int>(known.size()),TRUE)==CSTR_EQUAL;
}
inline bool HideId(std::wstring_view id,uint32_t policy) noexcept {
    if((policy&Cut)&&(Equal(id,L"cut")||Equal(id,L"Windows.Cut")))return true;
    if((policy&Copy)&&(Equal(id,L"copy")||Equal(id,L"Windows.Copy")))return true;
    if((policy&Paste)&&(Equal(id,L"paste")||Equal(id,L"Windows.Paste")))return true;
    if((policy&PasteLink)&&(Equal(id,L"pastelink")||Equal(id,L"Windows.PasteLink")))return true;
    if((policy&CopyPath)&&(Equal(id,L"copyaspath")||Equal(id,L"copypath")||Equal(id,L"Windows.CopyAsPath")))return true;
    if((policy&Rename)&&(Equal(id,L"rename")||Equal(id,L"Windows.Rename")))return true;
    if((policy&Delete)&&(Equal(id,L"delete")||Equal(id,L"Windows.Delete")))return true;
    if((policy&Properties)&&(Equal(id,L"properties")||Equal(id,L"Windows.Properties")))return true;
    if((policy&Share)&&(Equal(id,L"share")||Equal(id,L"Windows.Share")||Equal(id,L"Windows.ModernShare")||Equal(id,L"Windows.ModernShareFlyout")))return true;
    return (policy&New)&&(Equal(id,L"new")||Equal(id,L"newfolder")||Equal(id,L"Windows.New")||Equal(id,L"Windows.NewFolder"));
}
struct String {
    HSTRING value=nullptr;
    ~String(){if(value)WindowsDeleteString(value);}
    std::wstring_view View()const noexcept {UINT n=0;const auto text=WindowsGetStringRawBuffer(value,&n);return {text,n};}
};
inline HRESULT GetId(IInspectable* item,String& value) noexcept {
    using Getter=HRESULT(STDMETHODCALLTYPE*)(IInspectable*,HSTRING*);
    return reinterpret_cast<Getter>((*reinterpret_cast<void***>(item))[6])(item,&value.value);
}
inline HRESULT FilterModels(IUnknown* items,uint32_t policy) noexcept {
    if(!items||!(policy&builtinMask))return S_OK;
    Microsoft::WRL::ComPtr<ItemVector> vector;
    HRESULT hr=items->QueryInterface(itemVectorIid,reinterpret_cast<void**>(vector.GetAddressOf()));
    if(FAILED(hr))return hr;
    UINT count=0;hr=vector->Size(&count);if(FAILED(hr))return hr;
    bool changed=false;
    for(UINT i=count;i>0;--i){
        Microsoft::WRL::ComPtr<IInspectable> item;hr=vector->GetAt(i-1,&item);if(FAILED(hr))return hr;
        String id;hr=GetId(item.Get(),id);if(FAILED(hr))return hr;
        if(HideId(id.View(),policy)){hr=vector->RemoveAt(i-1);if(FAILED(hr))return hr;++removed;changed=true;}
    }
    if(!changed)return S_OK;
    hr=vector->Size(&count);if(FAILED(hr))return hr;
    // Trim only separators made redundant by filtering; preserve other native
    // layout tokens, unknown commands and the "show more options" action.
    bool nextSeparator=true;
    for(UINT i=count;i>0;--i){
        Microsoft::WRL::ComPtr<IInspectable> item;hr=vector->GetAt(i-1,&item);if(FAILED(hr))return hr;
        String id;hr=GetId(item.Get(),id);if(FAILED(hr))return hr;
        const bool separator=Equal(id.View(),L"Separator");
        if(separator&&(nextSeparator||i==1)){hr=vector->RemoveAt(i-1);if(FAILED(hr))return hr;}
        nextSeparator=separator;
    }
    return S_OK;
}
inline bool Policy(IUnknown* native,uint32_t& policy) noexcept {
    return enabled.load()&&policyProbe&&policyProbe(native,&policy);
}
inline IUnknown** __fastcall CreateModels(IUnknown* native,IUnknown** output,HMENU menu) {
    ++createDepth;
    struct DepthScope { ~DepthScope(){--createDepth;} } depthScope;
    if(expectedProjectionRoot&&menu==expectedProjectionRoot)expectedProjectionObserved=true;
    if(createDepth==1&&testInsertion&&FAILED(testInsertion(native,menu)))++failures;
    auto result=originalCreate(native,output,menu);
    if(createDepth==1&&output&&*output&&modelAugmenter&&FAILED(modelAugmenter(native,output)))++failures;
    uint32_t policy=0;
    if(output&&*output&&Policy(native,policy)&&(policy&builtinMask)){
        if(FAILED(FilterModels(*output,policy)))++failures;
    }
    return result;
}
inline bool HideExtension(const GUID& clsid,uint32_t policy) noexcept {
    return ((policy&SevenZip)&&(clsid==sevenZipModern||clsid==sevenZipVendor))||
        ((policy&Wps)&&clsid==wpsExtension);
}
inline void __fastcall InsertExtension(IUnknown* native,HMENU menu,const GUID* clsid,const HSTRING* package) {
    uint32_t policy=0;
    if(clsid&&Policy(native,policy)&&HideExtension(*clsid,policy)){++skippedExtensions;return;}
    // This is before the native loading placeholder and worker task are
    // created, so hidden extensions never activate or retain command objects.
    originalExtension(native,menu,clsid,package);
}
inline bool __fastcall UpdateModel(IUnknown* native,const HSTRING* id,const MENUITEMINFOW& item,ULONG flags) {
    uint32_t policy=0;
    if(id&&Policy(native,policy)&&(policy&builtinMask)){
        UINT count=0;const auto text=WindowsGetStringRawBuffer(*id,&count);
        if(HideId({text,count},policy)){++skippedUpdates;return true;}
    }
    return originalUpdate(native,id,item,flags);
}
inline HRESULT InstallVerified(HMODULE module,PolicyProbe probe,ModelAugmenter augmenter=nullptr) noexcept {
    if(!module||!probe)return E_INVALIDARG;
    AcquireSRWLockExclusive(&setupLock);
    HRESULT result=setupFailure;
    if(SUCCEEDED(result)&&enabled.load()){
        result=retainedModule==module&&policyProbe==probe&&modelAugmenter==augmenter?S_OK:E_UNEXPECTED;
        ReleaseSRWLockExclusive(&setupLock);return result;
    }
    struct Entry {ULONG_PTR rva;BYTE bytes[32];void* replacement;void** original;};
    Entry entries[]={
        {0x71f20,{0x48,0x89,0x5c,0x24,0x20,0x55,0x56,0x57,0x41,0x54,0x41,0x55,0x41,0x56,0x41,0x57,0x48,0x8d,0xac,0x24,0x70,0xfe,0xff,0xff,0x48,0x81,0xec,0x90,0x02,0x00,0x00,0x48},reinterpret_cast<void*>(&CreateModels),reinterpret_cast<void**>(&originalCreate)},
        {0x6fd28,{0x40,0x55,0x53,0x56,0x57,0x41,0x54,0x41,0x55,0x41,0x56,0x41,0x57,0x48,0x8d,0x6c,0x24,0xe1,0x48,0x81,0xec,0xc8,0x00,0x00,0x00,0x48,0x8b,0x05,0xf8,0x12,0x16,0x00},reinterpret_cast<void*>(&InsertExtension),reinterpret_cast<void**>(&originalExtension)},
        {0x716bc,{0x48,0x89,0x5c,0x24,0x10,0x48,0x89,0x74,0x24,0x20,0x57,0x41,0x54,0x41,0x55,0x41,0x56,0x41,0x57,0x48,0x81,0xec,0xe0,0x01,0x00,0x00,0x48,0x8b,0x05,0x63,0xf9,0x15},reinterpret_cast<void*>(&UpdateModel),reinterpret_cast<void**>(&originalUpdate)},
    };
    auto base=reinterpret_cast<BYTE*>(module);
    if(SUCCEEDED(result))for(const auto& entry:entries){
        BYTE bytes[32]{};
        if(!menu_command_agility::ReadBytes(base+entry.rva,bytes,sizeof(bytes))||std::memcmp(bytes,entry.bytes,sizeof(bytes))){result=HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);break;}
    }
    unsigned created=0;
    if(SUCCEEDED(result)){
        const auto status=MH_Initialize();
        if(status!=MH_OK&&status!=MH_ERROR_ALREADY_INITIALIZED)result=E_FAIL;
    }
    if(SUCCEEDED(result))for(const auto& entry:entries){
        if(MH_CreateHook(base+entry.rva,entry.replacement,entry.original)!=MH_OK){result=E_FAIL;break;}
        ++created;
    }
    if(SUCCEEDED(result)&&!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,reinterpret_cast<LPCWSTR>(base+entries[0].rva),&retainedModule))result=E_FAIL;
    if(FAILED(result)){
        for(unsigned i=0;i<created;++i)MH_RemoveHook(base+entries[i].rva);
    }else{
        policyProbe=probe;
        modelAugmenter=augmenter;
        for(const auto& entry:entries)if(MH_QueueEnableHook(base+entry.rva)!=MH_OK)result=E_FAIL;
        if(SUCCEEDED(result)&&MH_ApplyQueued()!=MH_OK)result=E_FAIL;
        if(SUCCEEDED(result))enabled.store(true);
        else for(const auto& entry:entries)MH_DisableHook(base+entry.rva);
        // Once activation is attempted, keep trampolines and module ownership
        // until process exit; native worker calls may already be in flight.
    }
    if(FAILED(result))setupFailure=result;
    ReleaseSRWLockExclusive(&setupLock);return result;
}
} // namespace menu_model_filter
