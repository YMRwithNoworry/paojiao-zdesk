#include <windows.h>
#include <shobjidl.h>
#include <shlwapi.h>
#include <string>
#include <atomic>
static constexpr CLSID clsid={0xb3489bd3,0x6c40,0x4aaa,{0x96,0x10,0xd7,0x27,0x54,0x25,0x26,0x27}};
class Command final:public IExplorerCommand {
    std::atomic<ULONG> refs{1};
public:
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID id,void** out)override{
        if(!out)return E_POINTER;*out=nullptr;
        if(id!=IID_IUnknown&&id!=IID_IExplorerCommand)return E_NOINTERFACE;
        *out=static_cast<IExplorerCommand*>(this);AddRef();return S_OK;
    }
    ULONG STDMETHODCALLTYPE AddRef()override{return ++refs;}
    ULONG STDMETHODCALLTYPE Release()override{auto n=--refs;if(!n)delete this;return n;}
    HRESULT STDMETHODCALLTYPE GetTitle(IShellItemArray*,LPWSTR* out)override{return SHStrDupW(L"ZSDesk issue27 extension",out);}
    HRESULT STDMETHODCALLTYPE GetIcon(IShellItemArray*,LPWSTR* out)override{if(out)*out=nullptr;return E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE GetToolTip(IShellItemArray*,LPWSTR* out)override{if(out)*out=nullptr;return E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE GetCanonicalName(GUID* out)override{if(!out)return E_POINTER;*out=clsid;return S_OK;}
    HRESULT STDMETHODCALLTYPE GetState(IShellItemArray*,BOOL,EXPCMDSTATE* out)override{if(!out)return E_POINTER;*out=ECS_ENABLED;return S_OK;}
    HRESULT STDMETHODCALLTYPE GetFlags(EXPCMDFLAGS* out)override{if(!out)return E_POINTER;*out=ECF_DEFAULT;return S_OK;}
    HRESULT STDMETHODCALLTYPE EnumSubCommands(IEnumExplorerCommand** out)override{if(out)*out=nullptr;return E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE Invoke(IShellItemArray* items,IBindCtx*)override{
        wchar_t marker[32768]{};if(!GetEnvironmentVariableW(L"ZSDESK_TEST_COMMAND_MARKER",marker,32768))return E_INVALIDARG;
        DWORD count=0;HRESULT hr=items?items->GetCount(&count):E_POINTER;if(FAILED(hr))return hr;
        std::wstring text;
        for(DWORD i=0;i<count;++i){IShellItem* item=nullptr;hr=items->GetItemAt(i,&item);if(FAILED(hr))return hr;
            PWSTR path=nullptr;hr=item->GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING,&path);item->Release();
            if(FAILED(hr))return hr;text+=path;text+=L"\n";CoTaskMemFree(path);
        }
        HANDLE file=CreateFileW(marker,GENERIC_WRITE,FILE_SHARE_READ,nullptr,CREATE_NEW,FILE_ATTRIBUTE_NORMAL,nullptr);
        if(file==INVALID_HANDLE_VALUE)return HRESULT_FROM_WIN32(GetLastError());DWORD written=0;
        const auto bytes=static_cast<DWORD>(text.size()*sizeof(wchar_t));bool okay=WriteFile(file,text.data(),bytes,&written,nullptr)&&written==bytes;
        CloseHandle(file);return okay?S_OK:E_FAIL;
    }
};
class Factory final:public IClassFactory{
    std::atomic<ULONG> refs{1};
public:
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID id,void** out)override{if(!out)return E_POINTER;*out=nullptr;if(id!=IID_IUnknown&&id!=IID_IClassFactory)return E_NOINTERFACE;*out=this;AddRef();return S_OK;}
    ULONG STDMETHODCALLTYPE AddRef()override{return ++refs;}
    ULONG STDMETHODCALLTYPE Release()override{auto n=--refs;if(!n)delete this;return n;}
    HRESULT STDMETHODCALLTYPE CreateInstance(IUnknown* outer,REFIID id,void** out)override{if(outer)return CLASS_E_NOAGGREGATION;auto value=new Command;auto hr=value->QueryInterface(id,out);value->Release();return hr;}
    HRESULT STDMETHODCALLTYPE LockServer(BOOL)override{return S_OK;}
};
STDAPI DllGetClassObject(REFCLSID id,REFIID iid,void** out){if(id!=clsid)return CLASS_E_CLASSNOTAVAILABLE;auto value=new Factory;auto hr=value->QueryInterface(iid,out);value->Release();return hr;}
STDAPI DllCanUnloadNow(){return S_FALSE;}
