#pragma once
#include <windows.h>
#include <intrin.h>
#include <cstring>
#include "MinHook.h"
#include "CommandAgility.h"

namespace menu_submenu_lifetime {
// In the verified presenter, InsertMenuItemForAppExtension attaches its local
// submenu and then destroys that same local handle. The attached parent must
// own it until normal menu teardown, including recursively inserted groups.
struct Insertion {
    HMENU parent;
    UINT position;
    Insertion* previous;
    inline static thread_local Insertion* current=nullptr;
    explicit Insertion(HMENU menu,UINT index):parent(menu),position(index),previous(current){current=this;}
    ~Insertion(){current=previous;}
    Insertion(const Insertion&)=delete;
};
inline bool Attached(HMENU parent,UINT position,HMENU child) noexcept {
    if(!parent||!child||parent==child||!IsMenu(parent)||!IsMenu(child))return false;
    const int count=GetMenuItemCount(parent);
    return count>0&&count<=2048&&position<static_cast<UINT>(count)&&GetSubMenu(parent,position)==child;
}
using Destructor=void(__fastcall*)(HMENU*);
inline Destructor original=nullptr;
inline void* target=nullptr;
inline const void* attachedCleanupReturn=nullptr;
inline bool enabled=false;
inline constexpr BYTE entryBytes[]={0x48,0x8b,0x09,0x48,0x85,0xc9,0x75,0x02,0xc3,0xcc};
__declspec(noinline) inline void __fastcall DestroyLocal(HMENU* value) noexcept {
    // Restrict the ownership transfer to the exact native insertion cleanup,
    // on this thread, and a live child actually attached to that insertion.
    // Failed insertions and unrelated menu owners retain ordinary destruction.
    auto scope=Insertion::current;
    const DWORD error=GetLastError();
    const bool transferred=value&&scope&&_ReturnAddress()==attachedCleanupReturn&&Attached(scope->parent,scope->position,*value);
    SetLastError(error);
    if(transferred)return;
    original(value);
}
// Called under the command bridge setup lock after full-image verification.
inline HRESULT InstallVerified(HMODULE module) noexcept {
    void* entry=reinterpret_cast<BYTE*>(module)+0x55494;
    if(target&&target!=entry)return HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);
    if(enabled)return S_OK;
    BYTE bytes[sizeof(entryBytes)]{};
    if(!menu_command_agility::ReadBytes(entry,bytes,sizeof(bytes))||std::memcmp(bytes,entryBytes,sizeof(bytes)))
        return HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);
    if(!target){
        if(MH_CreateHook(entry,reinterpret_cast<void*>(&DestroyLocal),reinterpret_cast<void**>(&original))!=MH_OK||!original)return E_FAIL;
        target=entry;attachedCleanupReturn=reinterpret_cast<BYTE*>(module)+0x75440;
    }
    if(MH_EnableHook(target)!=MH_OK)return E_FAIL;
    enabled=true;return S_OK;
}
inline HRESULT Disable() noexcept {
    if(!enabled)return S_OK;
    if(MH_DisableHook(target)!=MH_OK)return E_FAIL;
    BYTE bytes[sizeof(entryBytes)]{};
    if(!menu_command_agility::ReadBytes(target,bytes,sizeof(bytes))||std::memcmp(bytes,entryBytes,sizeof(bytes)))return E_FAIL;
    enabled=false;return S_OK;
}
} // namespace menu_submenu_lifetime
