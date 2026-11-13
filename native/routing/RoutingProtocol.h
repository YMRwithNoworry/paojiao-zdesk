#pragma once
#include <windows.h>
#include <cstdint>

namespace routing {
inline constexpr DWORD kMagic = 0x3152464E;
inline constexpr DWORD kVersion = 1;
inline constexpr ULONG_PTR kCopyDataTag = 0x3152464E;
inline constexpr ULONG_PTR kActivateCopyDataTag = 0x5A534441;
inline constexpr wchar_t kActivationProperty[] = L"ZSDesk.Activate.v1";
inline constexpr wchar_t kBridgeClass[] = L"Native360.FileRevealBridge.1";
inline constexpr DWORD kMaxItems = 128;
inline constexpr DWORD kMaxPacketBytes = 1024 * 1024;

// UTF-16 payload: folder NUL followed by itemCount absolute item paths, each NUL.
// The receiver returns 1 only after copying and accepting the entire request.
// A zero result means no request was accepted. A timeout is indeterminate.
struct PacketHeader {
    DWORD magic = kMagic;
    DWORD version = kVersion;
    DWORD bytes = sizeof(PacketHeader);
    DWORD itemCount = 0;
    DWORD sourcePid = 0;
    DWORD sourceKind = 0;
    ULONGLONG requestId = 0;
};
static_assert(sizeof(PacketHeader) == 32);

// Export ABI: DWORD WINAPI Install(void*), Uninstall(void*), GetStatus(Status*).
// An uninstall disables future routing; it never unloads an in-use DLL.
struct Status {
    DWORD magic = kMagic;
    DWORD version = kVersion;
    DWORD bytes = sizeof(Status);
    DWORD role = 0; // 1 bridge, 2 Codex, 3 Weixin, 4 isolated validation host.
    DWORD installed = 0;
    DWORD activeCalls = 0;
    DWORD received = 0;
    DWORD completed = 0;
    DWORD failed = 0;
    DWORD declined = 0;
    DWORD indeterminate = 0;
    DWORD lastError = 0;
    DWORD queued = 0;
    DWORD lastSourceKind = 0;
    DWORD lastItemCount = 0;
    DWORD reserved = 0; // RoutingDiagnostics::Reason; zero for older adapters.
    ULONGLONG lastId = 0;
    ULONG_PTR bridgeWindow = 0;
    wchar_t lastFolder[1024]{};
};
static_assert(sizeof(Status) == 2128);

enum class ProbeKind : DWORD {
    SelectSingle = 1,
    ExplorerSelectW = 2,
    ExplorerSelectEx = 3,
    OpenDirectory = 4,
    SelectMultiple = 5,
    SelectAbsoluteChild = 6,
    FolderViaIdList = 7,
    ProcessExplorerW = 10,
    ProcessExplorerA = 11,
    WinExecExplorer = 12,
};
struct Probe {
    DWORD magic = kMagic;
    DWORD version = kVersion;
    DWORD bytes = sizeof(Probe);
    ProbeKind kind = ProbeKind::SelectSingle;
    HRESULT result = E_PENDING;
    DWORD error = 0;
    wchar_t path[2048]{};
    wchar_t secondPath[2048]{};
};
static_assert(sizeof(Probe) == 8216);
} // namespace routing
