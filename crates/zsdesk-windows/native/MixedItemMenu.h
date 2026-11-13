#pragma once
#include <windows.h>
#include <shlobj.h>
#include <shobjidl.h>
#include <shlguid.h>
#include <wrl/client.h>
#include <algorithm>
#include <cstring>
#include <cwchar>
#include <iterator>
#include <new>
#include <climits>
#include <string>
#include <vector>

// Public Shell interfaces only. Absolute identities remain intact for selections
// whose members are not siblings in the Shell namespace.
namespace mixed_menu {
using Microsoft::WRL::ComPtr;
inline HRESULT LastError(){const DWORD error=GetLastError();return error?HRESULT_FROM_WIN32(error):E_FAIL;}
inline DWORD DeleteFlags(DWORD invocation){return FOFX_SHOWELEVATIONPROMPT|((invocation&CMIC_MASK_SHIFT_DOWN)?0:(FOF_ALLOWUNDO|FOFX_ADDUNDORECORD));}
struct Command { const wchar_t* label; const char* verb; const wchar_t* verbW; SFGAOF attribute; };
inline constexpr Command commands[]={
    {L"打开", "open", L"open", 0},
    {L"剪切", "cut", L"cut", SFGAO_CANMOVE},
    {L"复制", "copy", L"copy", SFGAO_CANCOPY},
    {L"复制文件地址", "copyaspath", L"copyaspath", 0},
    {L"删除", "delete", L"delete", SFGAO_CANDELETE},
    {L"属性", "properties", L"properties", SFGAO_HASPROPSHEET},
};

class Menu final:public IContextMenu3,public IObjectWithSite,public IObjectWithSelection {
    LONG refs_=1;
    ComPtr<IShellItemArray> selection_;
    ComPtr<IContextMenu> native_;
    ComPtr<IUnknown> site_;
    bool basic_=true;
    SFGAOF attributes_=0;
    void FindNativeCommands(HMENU menu,UINT first,UINT last,bool (&found)[6],unsigned depth=0){
        if(depth>16)return;
        for(int index=0;index<GetMenuItemCount(menu);++index){
            const UINT id=GetMenuItemID(menu,index);
            if(id>=first&&id<=last){
                wchar_t verb[128]{};
                if(SUCCEEDED(native_->GetCommandString(id-first,GCS_VERBW,nullptr,reinterpret_cast<char*>(verb),128))){
                    verb[127]=0;
                    for(unsigned command=0;command<std::size(commands);++command)if(!_wcsicmp(verb,commands[command].verbW))found[command]=true;
                }
            }
            if(const HMENU child=GetSubMenu(menu,index))FindNativeCommands(child,first,last,found,depth+1);
        }
    }
    bool NativeCommandsAvailable(HMENU menu,UINT first,UINT last,UINT flags){
        if(flags&CMF_DEFAULTONLY)return true;
        bool found[6]{};FindNativeCommands(menu,first,last,found);
        for(const unsigned command:{1u,2u,4u,5u})if((attributes_&commands[command].attribute)&&!found[command])return false;
        return true;
    }
public:
    HRESULT Initialize(HWND owner,UINT count,PCIDLIST_ABSOLUTE const* pidls,bool preferNative){
        if(!count||!pidls)return E_INVALIDARG;
        HRESULT hr=SHCreateShellItemArrayFromIDLists(count,const_cast<PCIDLIST_ABSOLUTE*>(pidls),&selection_);
        if(FAILED(hr))return hr;
        selection_->GetAttributes(SIATTRIBFLAGS_AND,SFGAO_CANMOVE|SFGAO_CANCOPY|SFGAO_CANDELETE|SFGAO_HASPROPSHEET,&attributes_);
        if(preferNative){
            ComPtr<IShellFolder> desktop;
            if(SUCCEEDED(SHGetDesktopFolder(&desktop))){
                DEFCONTEXTMENU def{};def.hwnd=owner;def.psf=desktop.Get();def.cidl=count;
                // Absolute PIDLs are relative to the namespace root (Desktop).
                // No child is rebound to a different item's physical parent.
                std::vector<PCUITEMID_CHILD> children;
                children.reserve(count);
                for(UINT index=0;index<count;++index)children.push_back(reinterpret_cast<PCUITEMID_CHILD>(pidls[index]));
                def.apidl=children.data();
                SHCreateDefaultContextMenu(&def,IID_PPV_ARGS(&native_));
            }
        }
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid,void** out)override{
        if(!out)return E_POINTER;*out=nullptr;
        if(iid==IID_IUnknown||iid==IID_IContextMenu||iid==IID_IContextMenu2||iid==IID_IContextMenu3)*out=static_cast<IContextMenu3*>(this);
        else if(iid==IID_IObjectWithSite)*out=static_cast<IObjectWithSite*>(this);
        else if(iid==IID_IObjectWithSelection)*out=static_cast<IObjectWithSelection*>(this);
        else return E_NOINTERFACE;
        AddRef();return S_OK;
    }
    ULONG STDMETHODCALLTYPE AddRef()override{return InterlockedIncrement(&refs_);}
    ULONG STDMETHODCALLTYPE Release()override{ULONG refs=InterlockedDecrement(&refs_);if(!refs)delete this;return refs;}
    HRESULT STDMETHODCALLTYPE SetSite(IUnknown* site)override{
        site_=site;
        ComPtr<IObjectWithSite> target;
        if(native_&&SUCCEEDED(native_.As(&target))){const HRESULT hr=target->SetSite(site);if(!basic_)return hr;}
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE GetSite(REFIID iid,void** out)override{if(!out)return E_POINTER;*out=nullptr;return site_?site_->QueryInterface(iid,out):E_NOINTERFACE;}
    HRESULT STDMETHODCALLTYPE SetSelection(IShellItemArray*)override{return E_ACCESSDENIED;}
    HRESULT STDMETHODCALLTYPE GetSelection(REFIID iid,void** out)override{return selection_->QueryInterface(iid,out);}
    HRESULT STDMETHODCALLTYPE QueryContextMenu(HMENU menu,UINT at,UINT first,UINT last,UINT flags)override{
        if(!menu||last<first)return E_INVALIDARG;
        const int before=GetMenuItemCount(menu);
        if(native_){
            const HRESULT hr=native_->QueryContextMenu(menu,at,first,last,flags);
            if(SUCCEEDED(hr)&&HRESULT_CODE(hr)>0&&NativeCommandsAvailable(menu,first,last,flags)){basic_=false;return hr;}
            // A failing provider may already have inserted part of its menu.
            while(GetMenuItemCount(menu)>before)if(!DeleteMenu(menu,at,MF_BYPOSITION))return E_UNEXPECTED;
        }
        basic_=true;
        const UINT count=(flags&CMF_DEFAULTONLY)?1:static_cast<UINT>(std::size(commands));
        if(last-first+1<count)return E_INVALIDARG;
        for(UINT index=0;index<count;++index){
            const auto& command=commands[index];
            const UINT disabled=command.attribute&&!(attributes_&command.attribute)?MF_GRAYED:MF_ENABLED;
            if(!InsertMenuW(menu,at+index,MF_BYPOSITION|MF_STRING|disabled,first+index,command.label))return LastError();
        }
        if(!(flags&CMF_DEFAULTONLY))InsertMenuW(menu,at+count,MF_BYPOSITION|MF_STRING|MF_GRAYED,0,L"部分扩展仅支持同一文件夹内的项目");
        return MAKE_HRESULT(SEVERITY_SUCCESS,0,count);
    }
    HRESULT STDMETHODCALLTYPE GetCommandString(UINT_PTR id,UINT type,UINT* reserved,LPSTR text,UINT length)override{
        if(!basic_)return native_->GetCommandString(id,type,reserved,text,length);
        if(id>=std::size(commands))return E_INVALIDARG;
        if(type==GCS_VALIDATEA||type==GCS_VALIDATEW)return S_OK;
        if(!text||!length)return E_INVALIDARG;
        if(type==GCS_VERBW){wcsncpy_s(reinterpret_cast<wchar_t*>(text),length,commands[id].verbW,_TRUNCATE);return S_OK;}
        if(type==GCS_VERBA){strncpy_s(text,length,commands[id].verb,_TRUNCATE);return S_OK;}
        return E_NOTIMPL;
    }
    HRESULT STDMETHODCALLTYPE HandleMenuMsg(UINT message,WPARAM wp,LPARAM lp)override{LRESULT result=0;return HandleMenuMsg2(message,wp,lp,&result);}
    HRESULT STDMETHODCALLTYPE HandleMenuMsg2(UINT message,WPARAM wp,LPARAM lp,LRESULT* result)override{
        if(result)*result=0;
        if(!basic_){
            ComPtr<IContextMenu3> third;if(SUCCEEDED(native_.As(&third)))return third->HandleMenuMsg2(message,wp,lp,result);
            ComPtr<IContextMenu2> second;if(SUCCEEDED(native_.As(&second)))return second->HandleMenuMsg(message,wp,lp);
        }
        return E_NOTIMPL;
    }
    HRESULT Clipboard(bool cut){
        ComPtr<IDataObject> data;HRESULT hr=selection_->BindToHandler(nullptr,BHID_DataObject,IID_PPV_ARGS(&data));if(FAILED(hr))return hr;
        HGLOBAL block=GlobalAlloc(GMEM_MOVEABLE,sizeof(DWORD));if(!block)return E_OUTOFMEMORY;
        auto effect=static_cast<DWORD*>(GlobalLock(block));if(!effect){GlobalFree(block);return E_OUTOFMEMORY;}
        *effect=cut?DROPEFFECT_MOVE:DROPEFFECT_COPY;GlobalUnlock(block);
        FORMATETC format{static_cast<CLIPFORMAT>(RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT)),nullptr,DVASPECT_CONTENT,-1,TYMED_HGLOBAL};
        STGMEDIUM value{};value.tymed=TYMED_HGLOBAL;value.hGlobal=block;
        hr=data->SetData(&format,&value,TRUE);if(FAILED(hr)){GlobalFree(block);return hr;}
        return OleSetClipboard(data.Get());
    }
    HRESULT CopyPaths(HWND owner){
        DWORD count=0;HRESULT hr=selection_->GetCount(&count);if(FAILED(hr))return hr;
        std::wstring paths;
        for(DWORD index=0;index<count;++index){
            ComPtr<IShellItem> item;PWSTR path=nullptr;hr=selection_->GetItemAt(index,&item);if(FAILED(hr))return hr;
            hr=item->GetDisplayName(SIGDN_FILESYSPATH,&path);if(FAILED(hr))hr=item->GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING,&path);if(FAILED(hr))return hr;
            if(index)paths+=L"\r\n";paths+=L'"';paths+=path;paths+=L'"';CoTaskMemFree(path);
        }
        HGLOBAL block=GlobalAlloc(GMEM_MOVEABLE,(paths.size()+1)*sizeof(wchar_t));if(!block)return E_OUTOFMEMORY;
        void* text=GlobalLock(block);if(!text){GlobalFree(block);return E_OUTOFMEMORY;}
        std::memcpy(text,paths.c_str(),(paths.size()+1)*sizeof(wchar_t));GlobalUnlock(block);
        if(!OpenClipboard(owner)){hr=LastError();GlobalFree(block);return hr;}
        if(!EmptyClipboard()||!SetClipboardData(CF_UNICODETEXT,block)){hr=LastError();GlobalFree(block);}else hr=S_OK;
        CloseClipboard();return hr;
    }
    HRESULT Execute(UINT id,HWND owner,int show,DWORD invocation){
        if(id>=std::size(commands))return E_INVALIDARG;
        if(commands[id].attribute&&!(attributes_&commands[id].attribute))return E_ACCESSDENIED;
        if(id==1||id==2)return Clipboard(id==1);
        if(id==3)return CopyPaths(owner);
        if(id==4){
            ComPtr<IFileOperation> operation;HRESULT hr=CoCreateInstance(CLSID_FileOperation,nullptr,CLSCTX_INPROC_SERVER,IID_PPV_ARGS(&operation));
            if(SUCCEEDED(hr))hr=operation->SetOwnerWindow(owner);
            if(SUCCEEDED(hr))hr=operation->SetOperationFlags(DeleteFlags(invocation));
            if(SUCCEEDED(hr))hr=operation->DeleteItems(selection_.Get());
            if(SUCCEEDED(hr))hr=operation->PerformOperations();
            BOOL aborted=FALSE;if(SUCCEEDED(hr))hr=operation->GetAnyOperationsAborted(&aborted);
            return SUCCEEDED(hr)&&aborted?E_ABORT:hr;
        }
        if(id==5){ComPtr<IDataObject> data;HRESULT hr=selection_->BindToHandler(nullptr,BHID_DataObject,IID_PPV_ARGS(&data));return FAILED(hr)?hr:SHMultiFileProperties(data.Get(),0);}
        DWORD count=0;HRESULT hr=selection_->GetCount(&count);if(FAILED(hr))return hr;
        for(DWORD index=0;index<count;++index){
            ComPtr<IShellItem> item;PIDLIST_ABSOLUTE pidl=nullptr;hr=selection_->GetItemAt(index,&item);if(FAILED(hr))return hr;
            hr=SHGetIDListFromObject(item.Get(),&pidl);if(FAILED(hr))return hr;
            SHELLEXECUTEINFOW info{};info.cbSize=sizeof(info);info.fMask=SEE_MASK_IDLIST|SEE_MASK_UNICODE;info.hwnd=owner;info.lpVerb=L"open";info.lpIDList=pidl;info.nShow=show;
            const BOOL opened=ShellExecuteExW(&info);const DWORD error=opened?ERROR_SUCCESS:GetLastError();CoTaskMemFree(pidl);
            if(!opened)return error?HRESULT_FROM_WIN32(error):E_FAIL;
        }
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE InvokeCommand(CMINVOKECOMMANDINFO* info)override{
        if(!info)return E_POINTER;
        if(!basic_)return native_->InvokeCommand(info);
        try{
            UINT id=UINT_MAX;
            if(IS_INTRESOURCE(info->lpVerb))id=LOWORD(reinterpret_cast<ULONG_PTR>(info->lpVerb));
            else for(UINT index=0;index<std::size(commands);++index){
                const auto extended=reinterpret_cast<CMINVOKECOMMANDINFOEX*>(info);
                const bool unicode=info->cbSize>=sizeof(CMINVOKECOMMANDINFOEX)&&(info->fMask&CMIC_MASK_UNICODE)&&extended->lpVerbW;
                if(unicode?!_wcsicmp(extended->lpVerbW,commands[index].verbW):!_stricmp(info->lpVerb,commands[index].verb)){id=index;break;}
            }
            return Execute(id,info->hwnd,info->nShow,info->fMask);
        }catch(const std::bad_alloc&){return E_OUTOFMEMORY;}catch(...){return E_UNEXPECTED;}
    }
};

inline HRESULT Create(HWND owner,UINT count,PCIDLIST_ABSOLUTE const* pidls,IContextMenu** out,bool preferNative=true){
    if(!out)return E_POINTER;*out=nullptr;
    auto menu=new(std::nothrow) Menu;if(!menu)return E_OUTOFMEMORY;
    const HRESULT hr=menu->Initialize(owner,count,pidls,preferNative);
    if(SUCCEEDED(hr))*out=menu;else menu->Release();return hr;
}
} // namespace mixed_menu
