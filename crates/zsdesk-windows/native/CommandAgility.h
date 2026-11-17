#pragma once
#include <windows.h>
#include <shobjidl.h>
#include <roapi.h>
#include <wrl/client.h>
#include <cstring>

namespace menu_command_agility {
constexpr size_t kVerbBytes=0x48;
constexpr size_t kMaximumVerbs=2048;
struct VectorRange { BYTE* begin; BYTE* end; BYTE* capacity; };
using IsCurrent=bool(*)(void*);

inline bool ReadBytes(const void* source,void* output,size_t bytes) noexcept {
    __try {std::memcpy(output,source,bytes); return true;}
    __except(EXCEPTION_EXECUTE_HANDLER) {return false;}
}

// The layout belongs to the exact guarded ContextMenuPresenter_Old image.
// The caller supplies the vector argument of InsertMenuItemForAppExtension.
inline bool ReadRange(void* storage,VectorRange& range,size_t& count) noexcept {
    if (!storage) return false;
    if (!ReadBytes(storage,&range,sizeof(range))) return false;
    const ULONG_PTR first=reinterpret_cast<ULONG_PTR>(range.begin);
    const ULONG_PTR last=reinterpret_cast<ULONG_PTR>(range.end);
    const ULONG_PTR capacity=reinterpret_cast<ULONG_PTR>(range.capacity);
    if (last<first || capacity<last || (last-first)%kVerbBytes || last-first>kVerbBytes*kMaximumVerbs) return false;
    count=static_cast<size_t>((last-first)/kVerbBytes);
    return true;
}

// Legacy vector-layout helper retained for compatibility probes. Production
// guarded commands use an FTM proxy and do not modify these private slots.
// Call on the worker that constructed the command, immediately after the
// original Insert returns and before its coroutine publishes the vector to UI.
// Only a newly inserted record containing this call's exact command is changed.
inline HRESULT ConvertInsertedCommand(void* storage,size_t first,IExplorerCommand* command,
        IsCurrent isCurrent,void* context,unsigned* changed=nullptr) {
    if (changed) *changed=0;
    if (!command || !isCurrent) return E_POINTER;
    if (!isCurrent(context)) return HRESULT_FROM_WIN32(ERROR_CANCELLED);
    Microsoft::WRL::ComPtr<IExplorerCommand> keepCommand(command);
    VectorRange range{};
    size_t count=0;
    if (!ReadRange(storage,range,count) || count<first) return HRESULT_FROM_WIN32(ERROR_INVALID_DATA);
    const size_t endLimit=count;
    HRESULT overall=S_FALSE;
    for (size_t index=first;index<endLimit;++index) {
        if (!isCurrent(context)) return HRESULT_FROM_WIN32(ERROR_CANCELLED);
        if (!ReadRange(storage,range,count) || index>=count) return HRESULT_FROM_WIN32(ERROR_RETRY);
        BYTE* entry=range.begin+index*kVerbBytes;
        auto raw=*reinterpret_cast<IExplorerCommand**>(entry+0x18);
        if (raw!=command || *reinterpret_cast<IAgileReference**>(entry+0x20)) continue;
        const UINT id=*reinterpret_cast<UINT*>(entry);
        Microsoft::WRL::ComPtr<IAgileReference> reference;
        HRESULT result=RoGetAgileReference(AGILEREFERENCE_DEFAULT,IID_IExplorerCommand,raw,&reference);
        if (FAILED(result) || !reference) return FAILED(result)?result:E_UNEXPECTED;
        if (!isCurrent(context)) return HRESULT_FROM_WIN32(ERROR_CANCELLED);
        // COM calls may reenter. Re-read storage instead of retaining pointers
        // into a vector that could have been reallocated during the COM call.
        if (!ReadRange(storage,range,count) || index>=count) return HRESULT_FROM_WIN32(ERROR_RETRY);
        entry=range.begin+index*kVerbBytes;
        if (*reinterpret_cast<UINT*>(entry)!=id ||
            *reinterpret_cast<IExplorerCommand**>(entry+0x18)!=raw ||
            *reinterpret_cast<IAgileReference**>(entry+0x20)) return HRESULT_FROM_WIN32(ERROR_RETRY);
        *reinterpret_cast<IAgileReference**>(entry+0x20)=reference.Detach();
        *reinterpret_cast<IExplorerCommand**>(entry+0x18)=nullptr;
        raw->Release(); // Retire the reference previously owned by +0x18.
        if (changed) ++*changed;
        overall=S_OK;
    }
    return overall;
}
} // namespace menu_command_agility
