#pragma once
#include "RouteRequest.h"
#include "../RoutingDiagnostics.h"

namespace shell_route {
inline bool SystemExplorer(std::wstring_view executable,std::wstring_view windowsDirectory){
    return !EqualPath(executable,L"explorer.exe")&&IsExplorer(executable,windowsDirectory);
}
inline bool LaunchToken(std::wstring_view line,std::wstring& executable,std::wstring& parameters){
    line=Trim(line);if(line.empty()||line.size()>=32767)return false;
    size_t end=0;
    if(line.front()==L'"'){
        end=line.find(L'"',1);if(end==std::wstring_view::npos||end==1)return false;
        executable.assign(line.substr(1,end-1));++end;
        if(end<line.size()&&line[end]!=L' '&&line[end]!=L'\t')return false;
    }else{
        end=line.find_first_of(L" \t");if(end==std::wstring_view::npos)end=line.size();
        executable.assign(line.substr(0,end));if(executable.find(L'"')!=std::wstring::npos)return false;
    }
    parameters.assign(Trim(line.substr(end)));return true;
}
inline bool ProcessContract(DWORD flags,const STARTUPINFOW& startup,routing::Reason& reason){
    constexpr DWORD allowed=CREATE_UNICODE_ENVIRONMENT|CREATE_NO_WINDOW|CREATE_DEFAULT_ERROR_MODE|NORMAL_PRIORITY_CLASS;
    if(flags&~allowed){reason=routing::Reason::UnsupportedFlags;return false;}
    if(startup.cb!=sizeof(startup)||startup.lpReserved||startup.lpDesktop||startup.cbReserved2||startup.lpReserved2||
       (startup.dwFlags&~(STARTF_USESHOWWINDOW|STARTF_USESTDHANDLES))){reason=routing::Reason::UnsupportedStartup;return false;}
    if((startup.dwFlags&STARTF_USESHOWWINDOW)&&!VisibleOpenCommand(startup.wShowWindow)){reason=routing::Reason::UnsupportedShow;return false;}
    return true;
}
// Resolution of a bare name is performed using CreateProcess's documented
// search order before this parser is called. Unknown switches retain Explorer.
inline bool ExplorerProcess(std::wstring_view application,std::wstring_view line,std::wstring_view resolved,
        std::wstring_view windows,RouteRequest& request,routing::Reason& reason){
    if(!SystemExplorer(resolved,windows)){reason=routing::Reason::NotSystemExplorer;return false;}
    std::wstring token,args;
    if(!LaunchToken(line,token,args)){reason=routing::Reason::UnsupportedArguments;return false;}
    if(!EqualPath(token,L"explorer.exe")&&!SystemExplorer(token,windows)){
        reason=routing::Reason::UnsupportedArguments;return false;
    }
    if(!application.empty()&&!EqualPath(application,L"explorer.exe")&&!SystemExplorer(application,windows)){
        reason=routing::Reason::NotSystemExplorer;return false;
    }
    if(ExplorerSelect(resolved,L"open",args,SW_SHOWNORMAL,windows,request)||
       ExplorerDirectory(resolved,L"open",args,SW_SHOWNORMAL,windows,request))return true;
    reason=routing::Reason::UnsupportedArguments;return false;
}
inline std::wstring QuoteArgument(std::wstring_view value){
    std::wstring quoted=L"\"";size_t slashes=0;
    for(wchar_t character:value){
        if(character==L'\\'){++slashes;continue;}
        if(character==L'"')quoted.append(slashes*2+1,L'\\');else quoted.append(slashes,L'\\');
        slashes=0;quoted+=character;
    }
    quoted.append(slashes*2,L'\\');quoted+=L'"';return quoted;
}
inline bool LaunchCommand(std::wstring_view controller,DWORD source,DWORD kind,ULONGLONG id,const RouteRequest& request,std::wstring& command){
    command=QuoteArgument(controller)+L" --route-launch "+std::to_wstring(source)+L" "+std::to_wstring(kind)+L" "+std::to_wstring(id)+L" "+QuoteArgument(request.folder);
    for(const auto& item:request.items)command+=L" "+QuoteArgument(item);
    return command.size()<32767;
}
}
