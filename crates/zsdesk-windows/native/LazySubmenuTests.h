#pragma once
#include "LazySubmenuInitialization.h"
#include <atomic>
#include <new>

namespace menu_lazy_test {
struct Menu{HMENU value=CreatePopupMenu();~Menu(){if(value)DestroyMenu(value);}};
class Provider final:public IContextMenu3{
    std::atomic<ULONG> refs_{1};
public:
    HMENU root=nullptr,child=nullptr,nested=nullptr;UINT calls=0,invokes=0,queries=0;bool third=true,replace=false;HRESULT status=S_OK;bool* cancel=nullptr;bool* queryCancel=nullptr;
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid,void** value)override{
        if(!value)return E_POINTER;*value=nullptr;
        ++queries;if(queryCancel)*queryCancel=false;
        if(iid==IID_IUnknown||iid==IID_IContextMenu||iid==IID_IContextMenu2||(third&&iid==IID_IContextMenu3))*value=static_cast<IContextMenu3*>(this);
        else return E_NOINTERFACE;AddRef();return S_OK;
    }
    ULONG STDMETHODCALLTYPE AddRef()override{return ++refs_;}
    ULONG STDMETHODCALLTYPE Release()override{const auto value=--refs_;if(!value)delete this;return value;}
    HRESULT STDMETHODCALLTYPE QueryContextMenu(HMENU,UINT,UINT,UINT,UINT)override{return S_OK;}
    HRESULT STDMETHODCALLTYPE InvokeCommand(LPCMINVOKECOMMANDINFO)override{++invokes;return S_OK;}
    HRESULT STDMETHODCALLTYPE GetCommandString(UINT_PTR,UINT,UINT*,LPSTR,UINT)override{return E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE HandleMenuMsg(UINT message,WPARAM wp,LPARAM lp)override{return HandleMenuMsg2(message,wp,lp,nullptr);}
    HRESULT STDMETHODCALLTYPE HandleMenuMsg2(UINT message,WPARAM wp,LPARAM,LRESULT* result)override{
        if(result)*result=0;if(message!=WM_INITMENUPOPUP)return E_NOTIMPL;++calls;
        if(cancel)*cancel=false;if(FAILED(status))return status;
        const auto menu=reinterpret_cast<HMENU>(wp);
        if(menu==child){
            if(replace){HMENU next=CreatePopupMenu();if(!next)return E_OUTOFMEMORY;
                MENUITEMINFOW item{};item.cbSize=sizeof(item);item.fMask=MIIM_SUBMENU;item.hSubMenu=next;
                if(!SetMenuItemInfoW(root,0,TRUE,&item)){DestroyMenu(next);return E_FAIL;}
                DestroyMenu(child);child=next;replace=false;return S_OK;}
            if(!AppendMenuW(menu,MF_STRING,41,L"First tool command"))return E_FAIL;
            nested=CreatePopupMenu();if(!nested)return E_OUTOFMEMORY;
            if(!AppendMenuW(menu,MF_POPUP,reinterpret_cast<UINT_PTR>(nested),L"Nested tools")){DestroyMenu(nested);nested=nullptr;return E_FAIL;}
        }else if(menu==nested){if(!AppendMenuW(menu,MF_STRING,42,L"Nested tool command"))return E_FAIL;}
        else return E_INVALIDARG;
        return S_OK;
    }
};
inline HRESULT Run(){
    for(bool third:{false,true})for(bool replace:{false,true}){
        Menu root;HMENU child=CreatePopupMenu();if(!root.value||!child)return E_OUTOFMEMORY;
        if(!AppendMenuW(root.value,MF_POPUP,reinterpret_cast<UINT_PTR>(child),L"Lazy tools")){DestroyMenu(child);return E_FAIL;}
        Microsoft::WRL::ComPtr<Provider> provider;provider.Attach(new(std::nothrow)Provider);if(!provider)return E_OUTOFMEMORY;
        provider->root=root.value;provider->child=child;provider->third=third;provider->replace=replace;std::unordered_set<HMENU> initialized;
        HRESULT result=menu_lazy::InitializeEmpty(provider.Get(),root.value,initialized);if(FAILED(result))return result;
        child=GetSubMenu(root.value,0);const UINT expected=replace?3:2;
        if(provider->calls!=expected||provider->invokes||GetMenuItemCount(child)!=2||GetMenuItemID(child,0)!=41
            ||!provider->nested||GetMenuItemCount(provider->nested)!=1||GetMenuItemID(provider->nested,0)!=42)return E_UNEXPECTED;
        result=menu_lazy::InitializeEmpty(provider.Get(),root.value,initialized);
        if(FAILED(result)||provider->calls!=expected||provider->invokes)return E_UNEXPECTED;
    }
    {
        Menu root;Microsoft::WRL::ComPtr<Provider> provider;provider.Attach(new(std::nothrow)Provider);if(!root.value||!provider)return E_OUTOFMEMORY;
        bool current=true;provider->queryCancel=&current;std::unordered_set<HMENU> initialized;
        auto live=[](void* value)->bool{return *static_cast<bool*>(value);};
        if(menu_lazy::InitializeEmpty(provider.Get(),root.value,initialized,live,&current)!=HRESULT_FROM_WIN32(ERROR_CANCELLED)
            ||provider->queries!=1||provider->calls||provider->invokes)return E_UNEXPECTED;
    }
    for(int scenario:{0,1,2}){
        const bool cancelled=scenario!=0;
        Menu root;HMENU child=CreatePopupMenu();if(!root.value||!child)return E_OUTOFMEMORY;
        if(!AppendMenuW(root.value,MF_POPUP,reinterpret_cast<UINT_PTR>(child),L"Unavailable tools")){DestroyMenu(child);return E_FAIL;}
        Microsoft::WRL::ComPtr<Provider> provider;provider.Attach(new(std::nothrow)Provider);if(!provider)return E_OUTOFMEMORY;
        provider->child=child;bool current=true;if(cancelled)provider->cancel=&current;else provider->status=E_ACCESSDENIED;
        if(scenario==2)provider->status=E_NOTIMPL;
        std::unordered_set<HMENU> initialized;auto live=[](void* value)->bool{return *static_cast<bool*>(value);};
        const HRESULT result=menu_lazy::InitializeEmpty(provider.Get(),root.value,initialized,live,&current);
        if(result!=(cancelled?HRESULT_FROM_WIN32(ERROR_CANCELLED):E_ACCESSDENIED)||provider->invokes||provider->calls!=1)return E_UNEXPECTED;
    }
    return S_OK;
}
} // namespace menu_lazy_test
