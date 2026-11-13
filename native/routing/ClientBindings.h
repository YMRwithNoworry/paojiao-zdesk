#pragma once
#include "support/RemoteAdapter.h"
#include "AdapterConfig.h"
#include <set>
#include <algorithm>

struct Spec {
    DWORD role=0,profile=1;
    bool enabled=false;
    std::wstring name,path,support;
    std::string hash,supportHash;
};
inline bool RoutingHash(std::wstring text,std::string& output){
    if(text.size()!=64)return false;output.clear();
    for(wchar_t c:text){if(c>=L'A'&&c<=L'F')c+=L'a'-L'A';if(!((c>=L'0'&&c<=L'9')||(c>=L'a'&&c<=L'f')))return false;output.push_back(char(c));}return true;
}
inline std::wstring RoutingIni(const std::wstring& file,const wchar_t* section,const wchar_t* key,const wchar_t* fallback=L""){
    wchar_t value[4096]{};DWORD size=GetPrivateProfileStringW(section,key,fallback,value,DWORD(std::size(value)),file.c_str());
    if(size>=std::size(value)-1)return {};return value;
}
inline bool RoutingLocalPath(const std::wstring& path){
    return path.size()>3&&path.size()<routing_config::kPathCharacters&&iswalpha(path[0])&&path[1]==L':'&&path[2]==wchar_t(92)&&path.find(L'"')==std::wstring::npos;
}
inline DWORD ReadClientBindings(const std::wstring& file,std::vector<Spec>& specs){
    specs.clear();DWORD count=GetPrivateProfileIntW(L"Clients",L"Count",0,file.c_str());if(count>128)return ERROR_INVALID_DATA;
    std::set<DWORD> ids;std::set<std::wstring> paths;
    for(DWORD i=0;i<count;++i){
        std::wstring section=L"Client."+std::to_wstring(i);Spec spec;
        spec.role=GetPrivateProfileIntW(section.c_str(),L"Id",0,file.c_str());
        spec.profile=GetPrivateProfileIntW(section.c_str(),L"Profile",0,file.c_str());
        spec.enabled=GetPrivateProfileIntW(section.c_str(),L"Enabled",0,file.c_str())!=0;
        spec.path=RoutingIni(file,section.c_str(),L"Executable");spec.support=RoutingIni(file,section.c_str(),L"CallerModule");
        if(spec.role<2||!ids.insert(spec.role).second||spec.profile<1||spec.profile>4||!RoutingLocalPath(spec.path)||!RoutingLocalPath(spec.support))return ERROR_INVALID_DATA;
        if(!RoutingHash(RoutingIni(file,section.c_str(),L"ExecutableSHA256"),spec.hash)||!RoutingHash(RoutingIni(file,section.c_str(),L"CallerSHA256"),spec.supportHash))return ERROR_INVALID_DATA;
        std::wstring lower=spec.path;std::transform(lower.begin(),lower.end(),lower.begin(),[](wchar_t c){return wchar_t(towlower(c));});if(!paths.insert(lower).second)return ERROR_INVALID_DATA;
        spec.name=BaseName(spec.path.c_str());specs.push_back(std::move(spec));
    }
    return 0;
}
