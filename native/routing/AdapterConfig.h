#pragma once
#include <windows.h>
#include <cstdint>

namespace routing_config {
inline constexpr DWORD kMagic=0x3242464E;
inline constexpr DWORD kVersion=2;
inline constexpr size_t kPathCharacters=2048;
// Compatibility contracts: a native executable calling documented Shell APIs,
// Chromium's chrome.dll, or WeChat's Weixin.dll. No private product offsets.
enum class Profile : DWORD { NativeShell=1, Chromium=2, WeChat=3, ExplicitModule=4 };
struct AdapterConfig {
    DWORD magic=kMagic,version=kVersion,bytes=sizeof(AdapterConfig),applicationId=0;
    Profile profile=Profile::NativeShell;
    DWORD reserved[3]{};
    wchar_t executable[kPathCharacters]{};
    char executableSHA256[65]{};
    wchar_t callerModule[kPathCharacters]{};
    char callerSHA256[65]{};
    wchar_t controller[kPathCharacters]{};
    char controllerSHA256[65]{};
    wchar_t receiverClass[128]=L"ZSDesk.FileRevealRouter.2";
};
static_assert(sizeof(AdapterConfig)<65536);
}
