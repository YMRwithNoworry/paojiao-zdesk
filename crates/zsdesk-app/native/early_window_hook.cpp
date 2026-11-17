#define WIN32_LEAN_AND_MEAN
#include <windows.h>

// No Shell, COM, allocations or background threads run in the caller. The
// recovery process owns every visibility change and acknowledges it before
// this synchronous show/activation notification returns.
static void signal(HWND window, bool explorer, UINT event) {
    if (!window || GetAncestor(window, GA_ROOT) != window) return;
    wchar_t name[64];
    if (!GetClassNameW(window, name, 64)) return;
    if (explorer ? (lstrcmpW(name, L"CabinetWClass") && lstrcmpW(name, L"ExploreWClass"))
                 : lstrcmpW(name, L"#32770")) return;
    HWND receiver = FindWindowW(explorer ? L"ZSDesk.EarlyExplorer.Guard" : L"ZSDesk.EarlyDialog.Guard", nullptr);
    if (!receiver) return;
    DWORD receiver_pid = 0;
    GetWindowThreadProcessId(receiver, &receiver_pid);
    if (receiver_pid == GetCurrentProcessId()) return;
    constexpr auto marker = L"ZSDesk.EarlyHook.InProgress";
    if (GetPropW(window, marker) || !SetPropW(window, marker, reinterpret_cast<HANDLE>(1))) return;
    DWORD_PTR result = 0;
    // Allow the source thread to service the guard's bounded WM_GETTEXT reads.
    SendMessageTimeoutW(receiver, WM_APP + 119, reinterpret_cast<WPARAM>(window), event,
                        SMTO_ABORTIFHUNG, 80, &result);
    RemovePropW(window, marker);
}
extern "C" __declspec(dllexport) void CALLBACK ZSDeskDialogEvent(HWINEVENTHOOK, DWORD event, HWND window, LONG object, LONG child, DWORD, DWORD) {
    if (object == OBJID_WINDOW && child == 0) {
        signal(window, false, event);
        wchar_t probe[1];
        if (event == EVENT_OBJECT_SHOW && GetAncestor(window, GA_ROOT) == window &&
            GetEnvironmentVariableW(L"ZSDESK_DIALOG_FIXTURE", probe, 1) && !GetPropW(window, L"ZSDesk.Test.FirstShowAlpha")) {
            BYTE alpha = 255; GetLayeredWindowAttributes(window, nullptr, &alpha, nullptr);
            SetPropW(window, L"ZSDesk.Test.FirstShowAlpha", reinterpret_cast<HANDLE>(static_cast<UINT_PTR>(alpha) + 1));
        }
    }
}
extern "C" __declspec(dllexport) void CALLBACK ZSDeskExplorerEvent(HWINEVENTHOOK, DWORD event, HWND window, LONG object, LONG child, DWORD, DWORD) {
    if (object == OBJID_WINDOW && child == 0) signal(window, true, event);
}
extern "C" __declspec(dllexport) LRESULT CALLBACK ZSDeskDialogCbt(int code, WPARAM wp, LPARAM lp) {
    if (code == HCBT_ACTIVATE) signal(reinterpret_cast<HWND>(wp), false, EVENT_OBJECT_SHOW);
    return CallNextHookEx(nullptr, code, wp, lp);
}
extern "C" __declspec(dllexport) LRESULT CALLBACK ZSDeskExplorerCbt(int code, WPARAM wp, LPARAM lp) {
    if (code == HCBT_ACTIVATE) signal(reinterpret_cast<HWND>(wp), true, EVENT_OBJECT_SHOW);
    return CallNextHookEx(nullptr, code, wp, lp);
}
extern "C" BOOL WINAPI DllMain(HINSTANCE, DWORD, LPVOID) { return TRUE; }
