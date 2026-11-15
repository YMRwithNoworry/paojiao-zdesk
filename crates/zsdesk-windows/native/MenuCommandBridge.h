#pragma once
#include <windows.h>
#include <shobjidl.h>
#include <wrl/client.h>
#include <atomic>
#include <cstring>
#include "MinHook.h"
#include "CommandAgility.h"
#include "NativeCommandGuard.h"
#include "MenuSubmenuLifetime.h"

namespace menu_command_bridge {
using InsertFunction = void(__fastcall*)(IUnknown*, HMENU, IExplorerCommand*, UINT, void*, const void*);
using OwnerProbe = bool(*)(IUnknown*, DWORD*, ULONGLONG*, std::shared_ptr<native_command_guard::Gate>*) noexcept;
inline InsertFunction originalInsert = nullptr;
inline OwnerProbe ownerProbe = nullptr;
inline SRWLOCK setupLock = SRWLOCK_INIT;
inline void* target = nullptr;
inline HMODULE retainedModule = nullptr;
inline std::atomic<bool> enabled{false};
inline std::atomic<unsigned> active{0}, converted{0}, failures{0};
inline HRESULT setupFailure = S_OK;
inline constexpr BYTE originalEntry[32] = {
    0x40,0x55,0x53,0x56,0x57,0x41,0x54,0x41,0x55,0x41,0x56,0x41,0x57,0x48,0x8d,0xac,
    0x24,0x08,0xff,0xff,0xff,0x48,0x81,0xec,0xf8,0x01,0x00,0x00,0x48,0x8b,0x05,0xed
};
struct Scope {
    IUnknown* native;
    DWORD owner;
    ULONGLONG generation;
    std::shared_ptr<native_command_guard::Gate> gate;
};
inline bool IsCurrent(void* context) noexcept {
    const auto scope = static_cast<Scope*>(context);
    DWORD owner = 0;
    ULONGLONG generation = 0;
    std::shared_ptr<native_command_guard::Gate> gate;
    return enabled.load() && ownerProbe &&
        ownerProbe(scope->native, &owner, &generation,&gate) &&gate&&scope->gate&&gate->Token()==scope->gate->Token()&&
        owner == scope->owner && generation == scope->generation&&gate->Current(generation);
}
inline void __fastcall Insert(IUnknown* native, HMENU menu, IExplorerCommand* command,
        UINT index, void* storage, const void* packageName) {
    ++active;
    struct ActiveScope { ~ActiveScope() { --active; } } activeScope;
    Scope scope{native, 0, 0,{}};
    const bool ours = enabled.load() && ownerProbe &&
        ownerProbe(native, &scope.owner, &scope.generation,&scope.gate)&&scope.gate;
    Microsoft::WRL::ComPtr<IUnknown> keepNative;
    Microsoft::WRL::ComPtr<IExplorerCommand> keepCommand;
    if (ours) { keepNative = native; keepCommand = command; }
    if(!ours){originalInsert(native,menu,command,index,storage,packageName);return;}
    if(!IsCurrent(&scope))return;
    Microsoft::WRL::ComPtr<IExplorerCommand> guarded;HRESULT result=E_UNEXPECTED;
    try{result=native_command_guard::WrapCommand(command,scope.gate,scope.generation,{},&guarded);}
    catch(const std::bad_alloc&){result=E_OUTOFMEMORY;}catch(...){result=E_UNEXPECTED;}
    if(FAILED(result)||!guarded){++failures;scope.gate->Failure(FAILED(result)?result:E_UNEXPECTED,"insert-wrap-command",&IID_IExplorerCommand,scope.generation);return;}
    if(!IsCurrent(&scope))return;
    menu_command_agility::VectorRange range{};size_t first=0;
    const bool hadRange=menu_command_agility::ReadRange(storage,range,first);
    // Both owner-STA and asynchronous insertions receive the public-interface
    // proxy before native storage retains it. No private vector fields change.
    {
        menu_submenu_lifetime::Insertion ownership(menu,index);
        originalInsert(native,menu,guarded.Get(),index,storage,packageName);
    }
    ++converted;
    // Read only the already-verified vector layout to associate a public command
    // ID. The proxy is safe before this read; an unbound bypass cannot trigger
    // a parent-intercepted action because the gate requires an identity.
    if(hadRange&&IsCurrent(&scope)){
        size_t count=0;if(!menu_command_agility::ReadRange(storage,range,count)||count<first){scope.gate->Failure(E_UNEXPECTED,"insert-vector-range",nullptr,scope.generation);++failures;return;}
        Microsoft::WRL::ComPtr<IUnknown> guardedIdentity;guarded.As(&guardedIdentity);
        for(size_t row=first;row<count;++row){
            if(!IsCurrent(&scope))return;
            if(!menu_command_agility::ReadRange(storage,range,count)||row>=count){scope.gate->Failure(E_UNEXPECTED,"insert-vector-generation",nullptr,scope.generation);++failures;return;}
            BYTE* entry=range.begin+row*menu_command_agility::kVerbBytes;
            const UINT id=*reinterpret_cast<UINT*>(entry);
            auto raw=*reinterpret_cast<IExplorerCommand**>(entry+0x18);
            auto agile=*reinterpret_cast<IAgileReference**>(entry+0x20);
            bool match=raw==guarded.Get();
            Microsoft::WRL::ComPtr<IAgileReference> keepAgile(agile);
            if(!match&&keepAgile){Microsoft::WRL::ComPtr<IUnknown> identity;if(SUCCEEDED(keepAgile->Resolve(IID_PPV_ARGS(&identity))))match=identity==guardedIdentity;}
            if(match){
                // Resolve can pump metadata updates. Never bind an ID from a
                // record that was replaced or moved during that COM call.
                if(!IsCurrent(&scope))return;
                if(!menu_command_agility::ReadRange(storage,range,count)||row>=count){scope.gate->Failure(E_UNEXPECTED,"bind-vector-generation",nullptr,scope.generation);++failures;return;}
                entry=range.begin+row*menu_command_agility::kVerbBytes;
                if(*reinterpret_cast<UINT*>(entry)!=id||*reinterpret_cast<IExplorerCommand**>(entry+0x18)!=raw
                    ||*reinterpret_cast<IAgileReference**>(entry+0x20)!=agile){scope.gate->Failure(HRESULT_FROM_WIN32(ERROR_RETRY),"bind-command-replaced",nullptr,scope.generation);++failures;return;}
                Microsoft::WRL::ComPtr<native_command_guard::ProxyIdentity> identity;
                HRESULT bound=guarded->QueryInterface(native_command_guard::kProxyIdentity,reinterpret_cast<void**>(identity.GetAddressOf()));
                if(SUCCEEDED(bound))bound=identity->BindCommand(id);
                if(FAILED(bound)){scope.gate->Failure(bound,"bind-command-id",nullptr,scope.generation);++failures;return;}
            }
        }
    }
}

// The caller first verifies the full Windows.UI.FileExplorer image hash and
// presenter vtable. The entry guard also rejects an unrelated existing hook.
inline HRESULT InstallVerified(HMODULE module, OwnerProbe probe) noexcept {
    if (!module || !probe) return E_INVALIDARG;
    AcquireSRWLockExclusive(&setupLock);
    HRESULT result = setupFailure;
    void* entry = reinterpret_cast<BYTE*>(module) + 0x75130;
    if (SUCCEEDED(result) && target && target != entry) result = HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);
    if (SUCCEEDED(result) && ownerProbe && ownerProbe != probe) result = E_UNEXPECTED;
    if (SUCCEEDED(result) && enabled.load()) {
        result = ownerProbe == probe ? S_OK : E_UNEXPECTED;
    } else if (SUCCEEDED(result)) {
        BYTE bytes[sizeof(originalEntry)]{};
        if (!menu_command_agility::ReadBytes(entry, bytes, sizeof(bytes)) ||
            std::memcmp(bytes, originalEntry, sizeof(bytes))) {
            result = HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);
        } else {
            if (!target) {
                MH_STATUS status = MH_Initialize();
                if (status != MH_OK && status != MH_ERROR_ALREADY_INITIALIZED) result = E_FAIL;
                if (SUCCEEDED(result)) {
                    status = MH_CreateHook(entry, reinterpret_cast<void*>(&Insert), reinterpret_cast<void**>(&originalInsert));
                    if (status != MH_OK || !originalInsert) result = E_FAIL;
                }
                if (SUCCEEDED(result)) {
                    target = entry;
                    // The retained trampoline and any already-entered callback
                    // must keep their native destination valid until process exit.
                    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                            reinterpret_cast<LPCWSTR>(entry), &retainedModule)) result = E_FAIL;
                }
                if (FAILED(result)) setupFailure = result;
            }
            if (SUCCEEDED(result)) {
                // Publish once before the first hook activation. Old callbacks
                // can remain in flight across a disable/re-enable cycle.
                if (!ownerProbe) ownerProbe = probe;
                result=menu_submenu_lifetime::InstallVerified(module);
            }
            if (SUCCEEDED(result)) {
                const MH_STATUS status = MH_EnableHook(target);
                if (status == MH_OK) enabled.store(true);
                else result = E_FAIL;
            }
        }
    }
    ReleaseSRWLockExclusive(&setupLock);
    return result;
}

inline HRESULT Disable() noexcept {
    AcquireSRWLockExclusive(&setupLock);
    if(active.load()) {ReleaseSRWLockExclusive(&setupLock);return HRESULT_FROM_WIN32(ERROR_BUSY);}
    HRESULT result = S_OK;
    if (enabled.load()) {
        const MH_STATUS status = MH_DisableHook(target);
        if (status != MH_OK && status != MH_ERROR_DISABLED) result = E_FAIL;
        BYTE bytes[sizeof(originalEntry)]{};
        if (SUCCEEDED(result) && (!menu_command_agility::ReadBytes(target, bytes, sizeof(bytes)) ||
                std::memcmp(bytes, originalEntry, sizeof(bytes)))) result = HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);
        if (SUCCEEDED(result)) enabled.store(false);
    }
    if(SUCCEEDED(result))result=menu_submenu_lifetime::Disable();
    // Do not remove the trampoline or unload either module while native tasks
    // may still hold references. Rollback disables behavior and restores bytes.
    ReleaseSRWLockExclusive(&setupLock);
    return result;
}
} // namespace menu_command_bridge
