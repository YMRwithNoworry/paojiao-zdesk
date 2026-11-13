#pragma once
// Included in the adapter's implementation namespace after its shared guards.

bool AnsiLengthSafe(const char* value,size_t& length){
    __try{length=value?strnlen_s(value,32768):0;return length<32768;}
    __except(EXCEPTION_EXECUTE_HANDLER){return false;}
}
bool CopyAnsi(const char* value,std::wstring& result){
    size_t length=0;if(!AnsiLengthSafe(value,length))return false;
    if(!length){result.clear();return true;}
    std::string bytes(length,'\0');if(!CopyMemorySafe(bytes.data(),value,length))return false;
    const UINT page=GetACP();const int count=MultiByteToWideChar(page,MB_ERR_INVALID_CHARS,bytes.data(),int(length),nullptr,0);if(!count)return false;
    result.resize(size_t(count));if(MultiByteToWideChar(page,MB_ERR_INVALID_CHARS,bytes.data(),int(length),result.data(),count)!=count)return false;
    BOOL substituted=FALSE;const DWORD flags=page==CP_UTF8?WC_ERR_INVALID_CHARS:WC_NO_BEST_FIT_CHARS;
    const int needed=WideCharToMultiByte(page,flags,result.data(),count,nullptr,0,nullptr,page==CP_UTF8?nullptr:&substituted);
    if(needed!=int(length)||substituted)return false;
    std::string roundtrip(size_t(needed),'\0');
    return WideCharToMultiByte(page,flags,result.data(),count,roundtrip.data(),needed,nullptr,page==CP_UTF8?nullptr:&substituted)==needed&&!substituted&&roundtrip==bytes;
}
bool AnsiEnvironmentContract(const void* environment,DWORD flags){
    if(!environment)return true;
    __try{
        if(flags&CREATE_UNICODE_ENVIRONMENT){const auto text=static_cast<const wchar_t*>(environment);for(size_t i=1;i<32767;++i)if(!text[i-1]&&!text[i])return true;}
        else{const auto text=static_cast<const char*>(environment);for(size_t i=1;i<32767;++i)if(!text[i-1]&&!text[i])return true;}
        return false;
    }__except(EXCEPTION_EXECUTE_HANDLER){return false;}
}
bool VerifiedExplorer(std::wstring_view file,std::wstring_view directory,bool applicationName,bool shellLookup){
    std::wstring expected=gWindows+L"\\explorer.exe",normalized;
    if(!shell_route::NormalizeAbsolute(expected,expected))return false;
    if(!shell_route::EqualPath(file,L"explorer.exe")){
        if(!shell_route::NormalizeAbsolute(file,normalized)||!shell_route::EqualPath(normalized,expected))return false;
        const DWORD attributes=GetFileAttributesW(normalized.c_str());return attributes!=INVALID_FILE_ATTRIBUTES&&!(attributes&FILE_ATTRIBUTE_DIRECTORY);
    }
    if(shellLookup)for(HKEY hive:{HKEY_CURRENT_USER,HKEY_LOCAL_MACHINE}){
        wchar_t app[32768]{};DWORD bytes=sizeof(app);
        LSTATUS error=RegGetValueW(hive,L"Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\explorer.exe",nullptr,RRF_RT_REG_SZ|RRF_RT_REG_EXPAND_SZ,nullptr,app,&bytes);
        if(error==ERROR_SUCCESS){
            const DWORD attributes=GetFileAttributesW(app);
            return attributes!=INVALID_FILE_ATTRIBUTES&&!(attributes&FILE_ATTRIBUTE_DIRECTORY)&&shell_route::NormalizeAbsolute(app,normalized)&&shell_route::EqualPath(normalized,expected);
        }
        if(error!=ERROR_FILE_NOT_FOUND&&error!=ERROR_PATH_NOT_FOUND)return false;
    }
    std::wstring current(directory);
    if(current.empty()){
        const DWORD needed=GetCurrentDirectoryW(0,nullptr);if(!needed||needed>32768)return false;
        current.resize(needed);const DWORD length=GetCurrentDirectoryW(needed,current.data());if(!length||length>=needed)return false;current.resize(length);
    }
    if(!shell_route::NormalizeAbsolute(current,current))return false;
    std::vector<std::wstring> directories;
    if(!applicationName){
        std::wstring executable=gBinding.executable;const auto slash=executable.find_last_of(L'\\');if(slash==std::wstring::npos)return false;
        directories.push_back(executable.substr(0,slash));
    }
    directories.push_back(current);
    if(!applicationName){
        wchar_t system[32768]{};const UINT n=GetSystemDirectoryW(system,UINT(std::size(system)));if(!n||n>=std::size(system))return false;
        directories.push_back(system);directories.push_back(gWindows+L"\\System");directories.push_back(gWindows);
    }
    for(const auto& path:directories){
        const std::wstring candidate=path+(path.back()==L'\\'?L"":L"\\")+L"explorer.exe";
        const DWORD attributes=GetFileAttributesW(candidate.c_str());
        if(attributes!=INVALID_FILE_ATTRIBUTES)return !(attributes&FILE_ATTRIBUTE_DIRECTORY)&&shell_route::NormalizeAbsolute(candidate,normalized)&&shell_route::EqualPath(normalized,expected);
        const DWORD error=GetLastError();if(error!=ERROR_FILE_NOT_FOUND&&error!=ERROR_PATH_NOT_FOUND)return false;
    }
    // A normal Windows installation was already checked before PATH is reached.
    return false;
}
bool BuildProcessRequest(const std::wstring& application,const std::wstring& line,const std::wstring& directory,
        DWORD flags,const STARTUPINFOW& startup,RouteRequest& request){
    // lpCurrentDirectory belongs to the child; executable resolution uses the
    // parent's current directory. The requested child cwd is forwarded intact.
    (void)directory;
    routing::Reason reason=routing::Reason::None;
    if(!shell_route::ProcessContract(flags,startup,reason)){RecordError(ERROR_NOT_SUPPORTED,reason);return false;}
    std::wstring token,args;if(!shell_route::LaunchToken(line,token,args)){RecordError(ERROR_NOT_SUPPORTED,routing::Reason::UnsupportedArguments);return false;}
    const std::wstring& executable=application.empty()?token:application;
    if(!VerifiedExplorer(executable,L"",!application.empty())){RecordError(ERROR_NOT_SUPPORTED,routing::Reason::NotSystemExplorer);return false;}
    if(!shell_route::ExplorerProcess(application,line,gWindows+L"\\explorer.exe",gWindows,request,reason)){RecordError(ERROR_NOT_SUPPORTED,reason);return false;}
    if(!ExistingRequest(request)){RecordError(ERROR_PATH_NOT_FOUND,routing::Reason::PathUnavailable);return false;}
    return true;
}
bool LaunchRouted(const RouteRequest& request,DWORD kind,LPSECURITY_ATTRIBUTES processAttributes,LPSECURITY_ATTRIBUTES threadAttributes,
        BOOL inherit,DWORD flags,LPVOID environment,LPCWSTR directory,const STARTUPINFOW& startup,LPPROCESS_INFORMATION information){
    if(!information)return false;
    PROCESS_INFORMATION readable{};if(!CopyMemorySafe(&readable,information,sizeof(readable))){RecordError(ERROR_NOACCESS,routing::Reason::InvalidInput);return false;}
    Handle image(CreateFileW(gBinding.controller,GENERIC_READ,FILE_SHARE_READ,nullptr,OPEN_EXISTING,FILE_ATTRIBUTE_NORMAL,nullptr));
    if(image.get()==INVALID_HANDLE_VALUE||!HashMatches(gBinding.controller,gBinding.controllerSHA256)){RecordError(ERROR_REVISION_MISMATCH,routing::Reason::ControllerChanged);return false;}
    const ULONGLONG id=(static_cast<ULONGLONG>(GetCurrentProcessId())<<32)|(gSerial.fetch_add(1)+1);
    std::wstring command;if(!shell_route::LaunchCommand(gBinding.controller,GetCurrentProcessId(),kind,id,request,command)){
        RecordError(ERROR_BUFFER_OVERFLOW,routing::Reason::InvalidInput);return false;
    }
    STARTUPINFOW copy=startup;
    // A real, short-lived forwarder is created. Its process/thread handles and
    // IDs are supplied directly by CreateProcess, including inherited handles.
    const BOOL created=gOriginalProcessW(gBinding.controller,command.data(),processAttributes,threadAttributes,inherit,
        flags|CREATE_NO_WINDOW,environment,directory,&copy,information);
    if(!created){RecordError(GetLastError(),routing::Reason::LaunchFailed);return false;}
    gReceived.fetch_add(1);gCompleted.fetch_add(1);
    PROCESS_INFORMATION child{};if(CopyMemorySafe(&child,information,sizeof(child)))AllowSetForegroundWindow(child.dwProcessId);
    AcquireSRWLockExclusive(&gStatusLock);gLastError=0;gLastReason=0;gLastId=id;gLastKind=kind;gLastCount=static_cast<DWORD>(request.items.size());
    wcsncpy_s(gLastFolder,request.folder.c_str(),_TRUNCATE);ReleaseSRWLockExclusive(&gStatusLock);
    return true;
}
BOOL WINAPI HookProcessW(LPCWSTR application,LPWSTR line,LPSECURITY_ATTRIBUTES processAttributes,LPSECURITY_ATTRIBUTES threadAttributes,
        BOOL inherit,DWORD flags,LPVOID environment,LPCWSTR directory,LPSTARTUPINFOW startup,LPPROCESS_INFORMATION information){
    const DWORD saved=GetLastError();
    if(gDepth)return gOriginalProcessW(application,line,processAttributes,threadAttributes,inherit,flags,environment,directory,startup,information);
    ActiveScope active(_ReturnAddress());if(!active)return gOriginalProcessW(application,line,processAttributes,threadAttributes,inherit,flags,environment,directory,startup,information);
    try{
        std::wstring app,args,cwd;STARTUPINFOW start{};RouteRequest request;
        if(startup&&CopyMemorySafe(&start,startup,sizeof(start))&&CopyString(application,app)&&CopyString(line,args)&&CopyString(directory,cwd)&&
           BuildProcessRequest(app,args,cwd,flags,start,request)&&LaunchRouted(request,10,processAttributes,threadAttributes,inherit,flags,environment,directory,start,information))return TRUE;
    }catch(...){gFailed.fetch_add(1);RecordError(ERROR_NOT_ENOUGH_MEMORY);}
    SetLastError(saved);return gOriginalProcessW(application,line,processAttributes,threadAttributes,inherit,flags,environment,directory,startup,information);
}
BOOL WINAPI HookProcessA(LPCSTR application,LPSTR line,LPSECURITY_ATTRIBUTES processAttributes,LPSECURITY_ATTRIBUTES threadAttributes,
        BOOL inherit,DWORD flags,LPVOID environment,LPCSTR directory,LPSTARTUPINFOA startup,LPPROCESS_INFORMATION information){
    const DWORD saved=GetLastError();
    if(gDepth)return gOriginalProcessA(application,line,processAttributes,threadAttributes,inherit,flags,environment,directory,startup,information);
    ActiveScope active(_ReturnAddress());if(!active)return gOriginalProcessA(application,line,processAttributes,threadAttributes,inherit,flags,environment,directory,startup,information);
    try{
        std::wstring app,args,cwd,title;STARTUPINFOA start{};RouteRequest request;
        if(startup&&CopyMemorySafe(&start,startup,sizeof(start))&&start.cb==sizeof(start)&&!start.lpReserved&&!start.lpDesktop&&AnsiEnvironmentContract(environment,flags)&&
           CopyAnsi(application,app)&&CopyAnsi(line,args)&&CopyAnsi(directory,cwd)&&CopyAnsi(start.lpTitle,title)){
            static_assert(sizeof(STARTUPINFOA)==sizeof(STARTUPINFOW));STARTUPINFOW wide{};memcpy(&wide,&start,sizeof(wide));wide.lpTitle=start.lpTitle?title.data():nullptr;
            if(BuildProcessRequest(app,args,cwd,flags,wide,request)&&LaunchRouted(request,11,processAttributes,threadAttributes,inherit,flags,environment,directory?cwd.c_str():nullptr,wide,information))return TRUE;
        }else RecordError(ERROR_NO_UNICODE_TRANSLATION,routing::Reason::InvalidEncoding);
    }catch(...){gFailed.fetch_add(1);RecordError(ERROR_NOT_ENOUGH_MEMORY);}
    SetLastError(saved);return gOriginalProcessA(application,line,processAttributes,threadAttributes,inherit,flags,environment,directory,startup,information);
}
UINT WINAPI HookWinExec(LPCSTR command,UINT show){
    const DWORD saved=GetLastError();
    if(gDepth)return gOriginalWinExec(command,show);
    ActiveScope active(_ReturnAddress());if(!active)return gOriginalWinExec(command,show);
    try{
        std::wstring line;STARTUPINFOW startup{};startup.cb=sizeof(startup);startup.dwFlags=STARTF_USESHOWWINDOW;startup.wShowWindow=static_cast<WORD>(show);
        RouteRequest request;PROCESS_INFORMATION child{};
        if(show<=0xffff&&CopyAnsi(command,line)&&BuildProcessRequest(L"",line,L"",0,startup,request)&&
           LaunchRouted(request,12,nullptr,nullptr,FALSE,0,nullptr,nullptr,startup,&child)){
            CloseHandle(child.hThread);CloseHandle(child.hProcess);SetLastError(ERROR_SUCCESS);return 33;
        }
    }catch(...){gFailed.fetch_add(1);RecordError(ERROR_NOT_ENOUGH_MEMORY);}
    SetLastError(saved);return gOriginalWinExec(command,show);
}
