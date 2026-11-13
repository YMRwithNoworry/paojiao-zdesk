#pragma once
#include <windows.h>
#include <shobjidl.h>
#include <algorithm>
#include <cstdint>
#include <cwchar>
#include <vector>

namespace menu_dispatch {
enum Kind : uint32_t { Dismissed=0, Application=1, Rename=2, Open=3, Explore=4, Invoked=5, Expanded=6, NativeHandled=7 };
enum Intercept : uint32_t { InterceptRename=1, InterceptOpen=2 };
enum Phase : uint32_t { BeforePresentation=0, Presented=1, Executing=2 };
struct Result { uint32_t kind=Dismissed,command=0;HRESULT status=S_OK;uint32_t phase=BeforePresentation; };
static_assert(sizeof(Result)==16,"Menu result ABI");
using BeforeInvoke=HRESULT(WINAPI*)(void*);

inline HRESULT NumericOffset(UINT command,UINT first,UINT* offset) noexcept {
    if(!offset)return E_POINTER;
    if(command<first||command-first>0xffff)return E_INVALIDARG;
    *offset=command-first;return S_OK;
}
inline HRESULT InvokeNumeric(IContextMenu* menu,HWND owner,UINT command,UINT first,POINT point,bool shift,bool control) {
    if(!menu)return E_POINTER;
    UINT offset=0;const HRESULT valid=NumericOffset(command,first,&offset);if(FAILED(valid))return valid;
    CMINVOKECOMMANDINFOEX info{};info.cbSize=sizeof(info);
    info.fMask=CMIC_MASK_UNICODE|CMIC_MASK_PTINVOKE;
    if(shift)info.fMask|=CMIC_MASK_SHIFT_DOWN;
    if(control)info.fMask|=CMIC_MASK_CONTROL_DOWN;
    info.hwnd=owner;info.lpVerb=MAKEINTRESOURCEA(offset);info.lpVerbW=MAKEINTRESOURCEW(offset);
    info.nShow=SW_SHOWNORMAL;info.ptInvoke=point;
    return menu->InvokeCommand(reinterpret_cast<CMINVOKECOMMANDINFO*>(&info));
}
inline Result Dispatch(IContextMenu* menu,HWND owner,UINT command,UINT first,POINT point,
        bool shift,bool control,const std::vector<UINT>& applications,uint32_t intercept,
        BeforeInvoke before,void* beforeContext) {
    // The native presenter reports dismissal through Invoke(0) before its
    // dismiss callback. Zero is not a Shell command ID in the hosted menu.
    if(!command)return {Dismissed,0,S_OK,Presented};
    Result result{Invoked,command,S_OK,Presented};
    if(std::find(applications.begin(),applications.end(),command)!=applications.end()){
        result.kind=Application;return result;
    }
    UINT offset=0;result.status=NumericOffset(command,first,&offset);
    if(FAILED(result.status)||!menu){if(!menu)result.status=E_POINTER;return result;}
    wchar_t verb[128]{};
    if(SUCCEEDED(menu->GetCommandString(offset,GCS_VERBW,nullptr,reinterpret_cast<char*>(verb),128))){
        verb[127]=L'\0';
        if((intercept&InterceptRename)&&!_wcsicmp(verb,L"rename")){result.kind=Rename;return result;}
        if(intercept&InterceptOpen){
            if(!_wcsicmp(verb,L"open")){result.kind=Open;return result;}
            if(!_wcsicmp(verb,L"explore")){result.kind=Explore;return result;}
        }
    }
    result.phase=Executing;
    if(before){result.status=before(beforeContext);if(FAILED(result.status))return result;}
    result.status=InvokeNumeric(menu,owner,command,first,point,shift,control);
    return result;
}

// IsContextMenuOpen becomes false at the beginning of the closing animation.
// The command is delivered afterwards, before the native dismiss callback.
// Only that completion callback (or owner/loop shutdown) ends tracking.
struct Completion {
    enum Wait { Continue, Complete, Aborted, TimedOut };
    bool done=false;
    bool requested=false;
    UINT requestedCommand=0;
    bool closing=false;
    ULONGLONG closingSince=0;
    void Requested(UINT command) noexcept {requested=command!=0;requestedCommand=command;}
    void Dismissed() noexcept {done=true;}
    bool ClosingExpired(bool open,ULONGLONG now) noexcept {
        if(done||open){closing=false;return false;}
        if(!closing){closing=true;closingSince=now;}
        return now-closingSince>=5000;
    }
    Wait Poll(bool ownerAlive,bool quitting,bool open,ULONGLONG now) noexcept {
        if(!ownerAlive||quitting)return Aborted;
        if(done)return Complete;
        return ClosingExpired(open,now)?TimedOut:Continue;
    }
};
} // namespace menu_dispatch
