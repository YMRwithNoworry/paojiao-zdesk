#pragma once
#include <windows.h>
#include <vector>

namespace routing {
enum class Reason:DWORD {
    None=0,CallerNotBound=1,UnsupportedVerb=2,UnsupportedShow=3,NotSystemExplorer=4,
    UnsupportedArguments=5,UnsupportedFlags=6,UnsupportedStartup=7,InvalidEncoding=8,
    PathUnavailable=9,ReceiverUnavailable=10,AccessDenied=11,HigherIntegrity=12,
    Indeterminate=13,InvalidInput=14,ControllerChanged=15,LaunchFailed=16,RestartRequired=17,
};
inline const wchar_t* ReasonName(Reason reason){
    switch(reason){
        case Reason::None:return L"none";case Reason::CallerNotBound:return L"caller-module-not-bound";
        case Reason::UnsupportedVerb:return L"unsupported-verb";case Reason::UnsupportedShow:return L"unsupported-show-mode";
        case Reason::NotSystemExplorer:return L"not-verified-system-explorer";case Reason::UnsupportedArguments:return L"unsupported-explorer-arguments";
        case Reason::UnsupportedFlags:return L"unsupported-creation-or-shell-flags";case Reason::UnsupportedStartup:return L"unsupported-startup-contract";
        case Reason::InvalidEncoding:return L"non-roundtripping-ansi";case Reason::PathUnavailable:return L"filesystem-target-unavailable";
        case Reason::ReceiverUnavailable:return L"verified-receiver-unavailable";case Reason::AccessDenied:return L"access-denied";
        case Reason::HigherIntegrity:return L"access-denied-higher-integrity";case Reason::Indeterminate:return L"timeout-not-replayed";
        case Reason::InvalidInput:return L"invalid-input";case Reason::ControllerChanged:return L"controller-identity-changed";
        case Reason::LaunchFailed:return L"forwarder-process-creation-failed";case Reason::RestartRequired:return L"application-restart-required";
    }return L"unknown";
}
inline DWORD ProcessIntegrity(HANDLE process,DWORD& integrity,DWORD* elevated=nullptr){
    integrity=0;if(elevated)*elevated=0;HANDLE token=nullptr;
    if(!OpenProcessToken(process,TOKEN_QUERY,&token))return GetLastError();
    struct CloseToken{HANDLE token;~CloseToken(){CloseHandle(token);}}close{token};
    DWORD bytes=0;GetTokenInformation(token,TokenIntegrityLevel,nullptr,0,&bytes);
    DWORD error=ERROR_INVALID_DATA;
    if(bytes&&bytes<=65536){
        std::vector<BYTE> data(bytes);
        if(GetTokenInformation(token,TokenIntegrityLevel,data.data(),bytes,&bytes)){
            auto sid=reinterpret_cast<TOKEN_MANDATORY_LABEL*>(data.data())->Label.Sid;
            if(IsValidSid(sid)&&*GetSidSubAuthorityCount(sid)){integrity=*GetSidSubAuthority(sid,*GetSidSubAuthorityCount(sid)-1);error=0;}
        }else error=GetLastError();
    }
    if(!error&&elevated){TOKEN_ELEVATION value{};if(GetTokenInformation(token,TokenElevation,&value,sizeof(value),&bytes))*elevated=value.TokenIsElevated;}
    return error;
}
}
