#pragma once
#include <windows.h>
#include <shobjidl.h>
#include <wrl/client.h>
#include <unordered_set>
#include <vector>

namespace menu_lazy {
using Microsoft::WRL::ComPtr;
using Current=bool(*)(void*);
inline HRESULT InitializeEmpty(IContextMenu* context,HMENU root,std::unordered_set<HMENU>& initialized,
        Current current=nullptr,void* state=nullptr){
    if(!context||!IsMenu(root))return E_INVALIDARG;
    if(current&&!current(state))return HRESULT_FROM_WIN32(ERROR_CANCELLED);
    ComPtr<IContextMenu3> third;ComPtr<IContextMenu2> second;
    context->QueryInterface(IID_PPV_ARGS(&third));
    if(current&&!current(state))return HRESULT_FROM_WIN32(ERROR_CANCELLED);
    context->QueryInterface(IID_PPV_ARGS(&second));
    if(current&&!current(state))return HRESULT_FROM_WIN32(ERROR_CANCELLED);
    if(!third&&!second)return S_FALSE;
    struct Level{HMENU menu;UINT depth;};std::vector<Level> pending{{root,0}};
    std::unordered_set<HMENU> visited;size_t items=0;
    while(!pending.empty()){
        const auto level=pending.back();pending.pop_back();
        if(current&&!current(state))return HRESULT_FROM_WIN32(ERROR_CANCELLED);
        if(!IsMenu(level.menu))return HRESULT_FROM_WIN32(ERROR_CANCELLED);
        if(!visited.insert(level.menu).second)continue;
        if(visited.size()>128||level.depth>16)return HRESULT_FROM_WIN32(ERROR_STACK_OVERFLOW);
        for(int position=0;position<GetMenuItemCount(level.menu);++position){
            if(++items>8192)return E_OUTOFMEMORY;
            HMENU child=GetSubMenu(level.menu,position);if(!child)continue;
            while(child&&GetMenuItemCount(child)==0&&initialized.insert(child).second){
                if(initialized.size()>128)return HRESULT_FROM_WIN32(ERROR_STACK_OVERFLOW);
                if(current&&!current(state))return HRESULT_FROM_WIN32(ERROR_CANCELLED);
                LRESULT result=0;HRESULT status=third?third->HandleMenuMsg2(WM_INITMENUPOPUP,
                    reinterpret_cast<WPARAM>(child),MAKELPARAM(position,FALSE),&result):E_NOINTERFACE;
                if(current&&!current(state))return HRESULT_FROM_WIN32(ERROR_CANCELLED);
                if((status==E_NOTIMPL||status==E_NOINTERFACE)&&second&&IsMenu(child)&&GetMenuItemCount(child)==0)
                    status=second->HandleMenuMsg(WM_INITMENUPOPUP,reinterpret_cast<WPARAM>(child),MAKELPARAM(position,FALSE));
                if(current&&!current(state))return HRESULT_FROM_WIN32(ERROR_CANCELLED);
                if(FAILED(status)&&status!=E_NOTIMPL&&status!=E_NOINTERFACE)return status;
                if(!IsMenu(level.menu))return HRESULT_FROM_WIN32(ERROR_CANCELLED);
                child=GetSubMenu(level.menu,position);
            }
            if(child)pending.push_back({child,level.depth+1});
        }
    }
    return (current&&!current(state))||!IsMenu(root)?HRESULT_FROM_WIN32(ERROR_CANCELLED):S_OK;
}
} // namespace menu_lazy
