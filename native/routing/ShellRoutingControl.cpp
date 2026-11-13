#include "support/RemoteAdapter.h"
#include "RoutingProtocol.h"
#include "RoutingDiagnostics.h"
#include "adapter/RouteRequest.h"
#include "AdapterConfig.h"
#include "ClientBindings.h"
#include "Compatibility.h"
#include <sddl.h>
#include <shellapi.h>
#include <winternl.h>
#include <algorithm>
#include <atomic>
#include <deque>
#include <map>
#include <mutex>
#include <set>
#include <sstream>
#include <thread>
#include <cstdio>
#include <tuple>
#include <climits>

namespace {
constexpr wchar_t kRunKey[] = L"Software\\Microsoft\\Windows\\CurrentVersion\\Run";
std::wstring gRunName=L"ZSDeskFileRevealRouting";
constexpr wchar_t kRouterClass[] = L"ZSDesk.FileRevealRouter.2";
constexpr wchar_t kLegacyClass[] = L"Native360.FileRevealRouter.1";
constexpr wchar_t kTargetClass[] = L"ZSDesk.Native.Explorer";
constexpr wchar_t kPackage[] = L"package.ini";
constexpr wchar_t kBackup[] = L"routing-startup-backup.bin";
constexpr wchar_t kWatchState[] = L"routing-watcher-state.tsv";
constexpr wchar_t kStatusState[] = L"routing-status.current.tsv";
std::vector<Spec> gSpecs;
std::string gControllerHash;
std::wstring gExe,gRoot,gPrefix;
std::vector<BYTE> gSid;
DWORD gSession=0;
bool gQuiet=false;
HANDLE gStop=nullptr,gWake=nullptr;
std::atomic<bool> gAccepting{false};
std::mutex gQueueMutex,gAuthMutex,gStatsMutex;
std::deque<std::vector<BYTE>> gQueue;
using RequestKey=std::tuple<DWORD,ULONGLONG,ULONGLONG>;
std::map<RequestKey,ULONGLONG> gSeen;
std::deque<RequestKey> gSeenOrder;
routing::Status gStats;
struct Target { DWORD pid=0,role=0; ULONGLONG created=0; std::wstring path; };
std::map<DWORD,Target> gAuthorized;
struct Attempt { unsigned count=0; DWORD result=ERROR_NOT_READY; ULONGLONG next=0; bool verified=false,applied=false; };
std::map<std::string,Attempt> gAttempts;

ULONGLONG TimeNumber(const FILETIME& t) { return (ULONGLONG(t.dwHighDateTime)<<32)|t.dwLowDateTime; }
ULONGLONG Now() { FILETIME t{};GetSystemTimeAsFileTime(&t);return TimeNumber(t); }
std::wstring Join(const wchar_t* file) { return gRoot+L"\\"+file; }
const Spec* Specification(DWORD role) { for(const auto& s:gSpecs)if(s.role==role)return &s;return nullptr; }
std::string Key(const Target& t) { return std::to_string(t.role)+":"+std::to_string(t.pid)+":"+std::to_string(t.created); }
void Out(const std::wstring& text) {
    if(gQuiet)return;
    std::wstring line=text+L"\r\n";HANDLE h=GetStdHandle(STD_OUTPUT_HANDLE);if(!h||h==INVALID_HANDLE_VALUE)return;
    DWORD mode=0,written=0;
    if(GetConsoleMode(h,&mode)){WriteConsoleW(h,line.c_str(),DWORD(line.size()),&written,nullptr);return;}
    int n=WideCharToMultiByte(CP_UTF8,0,line.data(),int(line.size()),nullptr,0,nullptr,nullptr);
    std::string bytes(size_t(n),'\0');WideCharToMultiByte(CP_UTF8,0,line.data(),int(line.size()),bytes.data(),n,nullptr,nullptr);WriteFile(h,bytes.data(),DWORD(bytes.size()),&written,nullptr);
}
std::wstring Hex(DWORD n) { wchar_t s[32]{};swprintf_s(s,L"0x%08lX",n);return s; }
DWORD ReadFileBytes(const std::wstring& path,std::vector<BYTE>& bytes) {
    Handle file(CreateFileW(path.c_str(),GENERIC_READ,FILE_SHARE_READ,nullptr,OPEN_EXISTING,FILE_ATTRIBUTE_NORMAL,nullptr));if(file.get()==INVALID_HANDLE_VALUE)return ErrorOr();
    LARGE_INTEGER size{};if(!GetFileSizeEx(file.get(),&size)||size.QuadPart<0||size.QuadPart>1048576)return ERROR_INVALID_DATA;
    bytes.resize(size_t(size.QuadPart));DWORD read=0;return bytes.empty()||(ReadFile(file.get(),bytes.data(),DWORD(bytes.size()),&read,nullptr)&&read==bytes.size())?0:ErrorOr(ERROR_READ_FAULT);
}
DWORD WriteAtomic(const std::wstring& path,const std::vector<BYTE>& bytes) {
    std::wstring pending=path+L".pending";Handle file(CreateFileW(pending.c_str(),GENERIC_WRITE,0,nullptr,CREATE_ALWAYS,FILE_ATTRIBUTE_NORMAL,nullptr));if(file.get()==INVALID_HANDLE_VALUE)return ErrorOr();
    DWORD written=0;if(!bytes.empty()&&(!WriteFile(file.get(),bytes.data(),DWORD(bytes.size()),&written,nullptr)||written!=bytes.size()))return ErrorOr(ERROR_WRITE_FAULT);
    if(!FlushFileBuffers(file.get()))return ErrorOr();file.reset();return MoveFileExW(pending.c_str(),path.c_str(),MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH)?0:ErrorOr();
}
DWORD WriteText(const wchar_t* file,const std::string& text) { return WriteAtomic(Join(file),std::vector<BYTE>(text.begin(),text.end())); }
DWORD OwnerSid(HANDLE process,std::vector<BYTE>& sid) {
    HANDLE h=nullptr;if(!OpenProcessToken(process,TOKEN_QUERY,&h))return ErrorOr();Handle token(h);DWORD size=0;GetTokenInformation(token.get(),TokenUser,nullptr,0,&size);if(!size)return ErrorOr();
    std::vector<BYTE> data(size);if(!GetTokenInformation(token.get(),TokenUser,data.data(),size,&size))return ErrorOr();PSID source=reinterpret_cast<TOKEN_USER*>(data.data())->User.Sid;sid.resize(GetLengthSid(source));return CopySid(DWORD(sid.size()),sid.data(),source)?0:ErrorOr();
}
bool SameOwner(HANDLE process,DWORD pid) {
    DWORD session=0;if(!ProcessIdToSessionId(pid,&session)||session!=gSession)return false;std::vector<BYTE> sid;return !OwnerSid(process,sid)&&EqualSid(gSid.data(),sid.data());
}
DWORD Initialize() {
    wchar_t exe[32768]{};DWORD length=GetModuleFileNameW(nullptr,exe,DWORD(std::size(exe)));if(!length||length>=std::size(exe))return ERROR_BAD_PATHNAME;
    gExe=exe;gRoot=gExe.substr(0,gExe.find_last_of(L"\\/"));DWORD error=OwnerSid(GetCurrentProcess(),gSid);if(error)return error;
    gRunName=RoutingIni(Join(kPackage),L"Routing",L"StartupName",L"ZSDeskFileRevealRouting");
    if(gRunName!=L"ZSDeskFileRevealRouting"&&gRunName!=L"Native360FileRevealRouting")return ERROR_BAD_CONFIGURATION;
    std::wstring bindings=RoutingIni(Join(kPackage),L"Routing",L"BindingsFile",L"clients.ini");
    if(bindings.empty()||bindings.find_first_of(L"\\/:")!=std::wstring::npos||bindings.find(wchar_t(34))!=std::wstring::npos||bindings.find(L"..")!=std::wstring::npos)return ERROR_BAD_CONFIGURATION;
    error=ReadClientBindings(Join(bindings.c_str()),gSpecs);if(error)return error;
    if(!FileSha256(gExe.c_str(),gControllerHash))return ERROR_READ_FAULT;
    if(!ProcessIdToSessionId(GetCurrentProcessId(),&gSession))return ErrorOr();LPWSTR sid=nullptr;if(!ConvertSidToStringSidW(gSid.data(),&sid))return ErrorOr();
    gPrefix=L"Local\\"+std::wstring(gRunName.c_str())+L"."+sid+L"."+std::to_wstring(gSession);LocalFree(sid);return 0;
}
struct OperationLock {
    Handle mutex;bool held=false;DWORD error=0;
    OperationLock() {
        mutex.reset(CreateMutexW(nullptr,FALSE,(gPrefix+L".Operation").c_str()));if(!mutex){error=ErrorOr();return;}
        DWORD wait=WaitForSingleObject(mutex.get(),15000);held=wait==WAIT_OBJECT_0||wait==WAIT_ABANDONED;if(!held)error=wait==WAIT_TIMEOUT?ERROR_BUSY:ErrorOr();
    }
    ~OperationLock(){if(held)ReleaseMutex(mutex.get());}
};
DWORD CommandLine(HANDLE process,std::wstring& line) {
    using Query=NTSTATUS(NTAPI*)(HANDLE,PROCESSINFOCLASS,PVOID,ULONG,PULONG);
    auto query=reinterpret_cast<Query>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"),"NtQueryInformationProcess"));if(!query)return ERROR_PROC_NOT_FOUND;
    ULONG bytes=0;query(process,static_cast<PROCESSINFOCLASS>(60),nullptr,0,&bytes);if(bytes<sizeof(UNICODE_STRING)||bytes>131072)return ERROR_INVALID_DATA;
    std::vector<BYTE> buffer(bytes);NTSTATUS status=query(process,static_cast<PROCESSINFOCLASS>(60),buffer.data(),bytes,&bytes);if(status<0)return ERROR_ACCESS_DENIED;
    auto* value=reinterpret_cast<UNICODE_STRING*>(buffer.data());uintptr_t begin=reinterpret_cast<uintptr_t>(buffer.data()),end=begin+buffer.size(),address=reinterpret_cast<uintptr_t>(value->Buffer);
    if(value->Length%2||address<begin||address>end||value->Length>end-address)return ERROR_INVALID_DATA;line.assign(value->Buffer,value->Length/2);return 0;
}
bool IsMain(HANDLE process) {
    std::wstring line;if(CommandLine(process,line))return false;int argc=0;LPWSTR* argv=CommandLineToArgvW(line.c_str(),&argc);if(!argv)return false;bool main=true;
    for(int i=1;i<argc;i++)if(!_wcsnicmp(argv[i],L"--type=",7)||!_wcsicmp(argv[i],L"--type")){main=false;break;}LocalFree(argv);return main;
}
DWORD Identify(HANDLE process,DWORD pid,DWORD role,Target& target) {
    const auto* spec=Specification(role);if(!spec||!SameOwner(process,pid)||!IsX64Process(process))return ERROR_ACCESS_DENIED;
    wchar_t path[32768]{};DWORD size=DWORD(std::size(path));FILETIME created{},exited{},kernel{},user{};
    if(!QueryFullProcessImageNameW(process,0,path,&size)||!GetProcessTimes(process,&created,&exited,&kernel,&user))return ErrorOr();
    if(_wcsicmp(path,spec->path.c_str())||!IsMain(process))return ERROR_NOT_SUPPORTED;
    target={pid,role,TimeNumber(created),path};return 0;
}
DWORD Scan(std::vector<Target>& targets) {
    Handle snapshot(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS,0));if(snapshot.get()==INVALID_HANDLE_VALUE)return ErrorOr();PROCESSENTRY32W entry{};entry.dwSize=sizeof(entry);if(!Process32FirstW(snapshot.get(),&entry))return ErrorOr();
    do { for(const auto& spec:gSpecs)if(!_wcsicmp(entry.szExeFile,spec.name.c_str())){
        Handle process(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,FALSE,entry.th32ProcessID));Target target;if(process&&!Identify(process.get(),entry.th32ProcessID,spec.role,target)){targets.push_back(target);break;}
    }}while(Process32NextW(snapshot.get(),&entry));return 0;
}
DWORD OpenTarget(const Target& target,Handle& process) {
    process.reset(OpenProcess(PROCESS_QUERY_INFORMATION|PROCESS_QUERY_LIMITED_INFORMATION|PROCESS_VM_READ|PROCESS_VM_WRITE|PROCESS_VM_OPERATION|PROCESS_CREATE_THREAD,FALSE,target.pid));if(!process)return ErrorOr();
    Target checked;DWORD error=Identify(process.get(),target.pid,target.role,checked);if(error)return error;return checked.created==target.created?0:ERROR_INVALID_DATA;
}
DWORD Package(DWORD role,std::wstring& path,std::string& digest) {
    bool bridge=role==1;wchar_t file[260]{},hash[80]{};
    GetPrivateProfileStringW(L"Routing",bridge?L"BridgeFile":L"RouterFile",bridge?L"360RevealBridge.dll":L"ZSDeskRevealRouter.dll",file,DWORD(std::size(file)),Join(kPackage).c_str());
    GetPrivateProfileStringW(L"Routing",bridge?L"BridgeSHA256":L"RouterSHA256",L"",hash,DWORD(std::size(hash)),Join(kPackage).c_str());
    if(!file[0]||wcsstr(file,L"..")||wcspbrk(file,L"\\/:\""))return ERROR_BAD_CONFIGURATION;
    size_t length=wcslen(file);if(length<5||_wcsicmp(file+length-4,L".dll")||wcslen(hash)!=64)return ERROR_BAD_CONFIGURATION;
    digest.clear();for(wchar_t c:hash){if(!c)break;if(c>=L'A'&&c<=L'F')c+=L'a'-L'A';if(!((c>=L'0'&&c<=L'9')||(c>=L'a'&&c<=L'f')))return ERROR_BAD_CONFIGURATION;digest.push_back(char(c));}
    path=Join(file);std::string actual;return FileSha256(path.c_str(),actual)&&actual==digest?0:ERROR_REVISION_MISMATCH;
}
DWORD VerifyTarget(const Target& target,const std::vector<RemoteModule>& modules) {
    const auto* spec=Specification(target.role);if(!spec)return ERROR_INVALID_PARAMETER;std::string digest;
    if(!FileSha256(spec->path.c_str(),digest)||digest!=spec->hash)return ERROR_REVISION_MISMATCH;
    if(!spec->support.empty()){if(!FindModule(modules,spec->support.c_str(),true))return ERROR_MOD_NOT_FOUND;if(!FileSha256(spec->support.c_str(),digest)||digest!=spec->supportHash)return ERROR_REVISION_MISMATCH;}
    auto compatibility=InspectCompatibility(spec->path,spec->support);
    if(!compatibility.supported||compatibility.profile!=spec->profile)return ERROR_NOT_SUPPORTED;
    return 0;
}
template<class T> DWORD RemoteBuffer(HANDLE process,uintptr_t module,const char* name,T& value,DWORD timeout=10000) {
    const DWORD expectedMagic=value.magic,expectedVersion=value.version;
    uintptr_t function=RemoteExport(process,module,name);if(!function)return ERROR_PROC_NOT_FOUND;void* remote=VirtualAllocEx(process,nullptr,sizeof(value),MEM_COMMIT|MEM_RESERVE,PAGE_READWRITE);if(!remote)return ErrorOr();SIZE_T copied=0;
    if(!WriteProcessMemory(process,remote,&value,sizeof(value),&copied)||copied!=sizeof(value)){DWORD error=ErrorOr(ERROR_WRITE_FAULT);VirtualFreeEx(process,remote,0,MEM_RELEASE);return error;}
    DWORD returned=0;bool pending=false;DWORD error=RemoteCall(process,function,remote,timeout,returned,pending);
    if(!error&&(!ReadProcessMemory(process,remote,&value,sizeof(value),&copied)||copied!=sizeof(value)))error=ErrorOr(ERROR_READ_FAULT);
    if(!pending)VirtualFreeEx(process,remote,0,MEM_RELEASE);if(error)return error;
    if(value.magic!=expectedMagic||value.version!=expectedVersion||value.bytes!=sizeof(T))return ERROR_INVALID_DATA;return returned;
}
const char* Export(DWORD role,int action) {
    if(role==1)return action==0?"BridgeInstall":action==1?"BridgeUninstall":"BridgeGetStatus";
    return action==0?"RouterInstall":action==1?"RouterUninstall":"RouterGetStatus";
}
struct BoundAdapter { uintptr_t base=0;bool legacy=false;std::wstring path; };
DWORD SelectAdapter(const Target& target,HANDLE process,const std::vector<RemoteModule>& modules,BoundAdapter& adapter,bool mayLoad) {
    std::wstring path;std::string digest;DWORD error=Package(target.role,path,digest);if(error)return error;
    auto legacyFile=RoutingIni(Join(kPackage),L"Routing",L"LegacyRouterFile");std::string legacyHash;
    bool legacyConfigured=!legacyFile.empty()&&legacyFile.find_first_of(L"\\/:") == std::wstring::npos&&legacyFile.find(L"..") == std::wstring::npos&&RoutingHash(RoutingIni(Join(kPackage),L"Routing",L"LegacyRouterSHA256"),legacyHash);
    uintptr_t portable=FindModule(modules,path.c_str(),true);BoundAdapter legacyAdapter;
    for(const auto& module:modules){
        if(!_wcsicmp(module.path.c_str(),path.c_str()))continue;
        const wchar_t* name=BaseName(module.path.c_str());
        if(_wcsicmp(name,L"FileRevealRouter.dll")&&_wcsicmp(name,L"ZSDeskRevealRouter.dll"))continue;
        std::string actual;
        if(legacyConfigured&&!_wcsicmp(module.path.c_str(),Join(legacyFile.c_str()).c_str())&&FileSha256(module.path.c_str(),actual)&&actual==legacyHash){
            routing::Status status;error=RemoteBuffer(process,module.base,"RouterGetStatus",status);if(error)return error;
            const auto* spec=Specification(target.role);if(!spec||(spec->profile!=2&&spec->profile!=3)||status.role!=spec->profile)return ERROR_NOT_SUPPORTED;
            legacyAdapter={module.base,true,module.path};continue;
        }
        return ERROR_ALREADY_EXISTS; // An unrelated or older router must not be injected twice.
    }
    if(portable&&legacyAdapter.base)return ERROR_ALREADY_EXISTS;
    if(portable){adapter={portable,false,path};return 0;}
    if(legacyAdapter.base){adapter=std::move(legacyAdapter);return 0;}
    adapter.path=path;if(!mayLoad)return 0;
    return LoadAdapter(process,target.pid,path,adapter.base);
}
DWORD TargetStatus(const Target& target,routing::Status& status,bool& loaded) {
    loaded=false;Handle process;DWORD error=OpenTarget(target,process);if(error)return error;std::vector<RemoteModule> modules;error=ReadModules(target.pid,modules);if(error)return error;
    BoundAdapter adapter;error=SelectAdapter(target,process.get(),modules,adapter,false);if(error)return error;if(!adapter.base)return 0;loaded=true;
    return RemoteBuffer(process.get(),adapter.base,"RouterGetStatus",status);
}
DWORD ConfigureAdapter(const Target& target,HANDLE process,uintptr_t module){
    const auto* spec=Specification(target.role);if(!spec)return ERROR_INVALID_PARAMETER;
    DWORD abi=0;DWORD error=CallScalar(process,module,"RouterGetAbi",abi);if(error)return error;if(abi!=routing_config::kVersion)return ERROR_REVISION_MISMATCH;
    routing_config::AdapterConfig config;config.applicationId=spec->role;config.profile=static_cast<routing_config::Profile>(spec->profile);
    if(wcscpy_s(config.executable,spec->path.c_str())||wcscpy_s(config.callerModule,spec->support.c_str())||wcscpy_s(config.controller,gExe.c_str())||
       strcpy_s(config.executableSHA256,spec->hash.c_str())||strcpy_s(config.callerSHA256,spec->supportHash.c_str())||strcpy_s(config.controllerSHA256,gControllerHash.c_str()))return ERROR_BUFFER_OVERFLOW;
    error=RemoteBuffer(process,module,"RouterConfigure",config);
    if(error==ERROR_BUSY){DWORD returned=0;error=CallScalar(process,module,"RouterUninstall",returned);if(error)return error;if(returned)return returned;error=RemoteBuffer(process,module,"RouterConfigure",config);}
    return error;
}
DWORD RestoreTarget(const Target& target);
DWORD ApplyTarget(const Target& target,bool force) {
    const auto* spec=Specification(target.role);if(!spec)return ERROR_INVALID_PARAMETER;
    if(!spec->enabled){DWORD error=RestoreTarget(target);std::lock_guard<std::mutex> lock(gAuthMutex);gAuthorized.erase(target.pid);return error;}
    auto& attempt=gAttempts[Key(target)];ULONGLONG now=GetTickCount64();if(!force){if(attempt.applied)return 0;if(attempt.count>=5||now<attempt.next)return attempt.result;}
    Handle process;DWORD error=OpenTarget(target,process);if(error)return error;
    std::vector<RemoteModule> modules;error=ReadModules(target.pid,modules);if(error)return error;
    if(!attempt.verified){error=VerifyTarget(target,modules);if(error){attempt.result=error;if(error!=ERROR_MOD_NOT_FOUND)attempt.count=5;return error;}attempt.verified=true;}
    ++attempt.count;attempt.next=now+3000;
    BoundAdapter adapter;error=SelectAdapter(target,process.get(),modules,adapter,true);
    if(!error&&!adapter.legacy)error=ConfigureAdapter(target,process.get(),adapter.base);
    DWORD returned=0;if(!error)error=CallScalar(process.get(),adapter.base,"RouterInstall",returned);
    if(!error&&returned!=ERROR_SUCCESS&&returned!=ERROR_ALREADY_EXISTS)error=returned;
    routing::Status status;if(!error)error=RemoteBuffer(process.get(),adapter.base,"RouterGetStatus",status);
    if(!error&&!status.installed)error=ERROR_NOT_READY;
    attempt.result=error;attempt.applied=!error;
    if(!error){std::lock_guard<std::mutex> lock(gAuthMutex);gAuthorized[target.pid]=target;}
    return error;
}
DWORD ApplyCycle(bool force) {
    OperationLock operation;if(operation.error)return operation.error;std::vector<Target> targets;DWORD error=Scan(targets);if(error)return error;DWORD first=0;std::set<std::string> live;
    for(const auto& target:targets){if(target.role==1)continue;live.insert(Key(target));if(gStop&&WaitForSingleObject(gStop,0)==WAIT_OBJECT_0)return ERROR_CANCELLED;DWORD result=ApplyTarget(target,force);if(force)Out(std::wstring(Specification(target.role)->name.c_str())+L" PID="+std::to_wstring(target.pid)+L" apply="+Hex(result));if(result&&!first)first=result;}
    for(auto it=gAttempts.begin();it!=gAttempts.end();)if(!live.count(it->first))it=gAttempts.erase(it);else ++it;
    {std::lock_guard<std::mutex> lock(gAuthMutex);for(auto it=gAuthorized.begin();it!=gAuthorized.end();){auto match=std::find_if(targets.begin(),targets.end(),[&](const Target& t){return t.pid==it->second.pid&&t.created==it->second.created;});if(match==targets.end())it=gAuthorized.erase(it);else ++it;}}
    return first;
}
bool AbsolutePath(const std::wstring& path) {
    if(path.empty()||path.size()>32767||path.find(L'"')!=std::wstring::npos||path.find(L'/')!=std::wstring::npos)return false;
    if(path.rfind(L"\\\\?\\",0)==0||path.rfind(L"\\\\.\\",0)==0)return false;
    return path.size()>=3&&((iswalpha(path[0])&&path[1]==L':'&&path[2]==L'\\')||(path[0]==L'\\'&&path[1]==L'\\'&&path[2]!=L'\\'));
}
bool Packet(const void* data,DWORD bytes,routing::PacketHeader& header,std::wstring* folder=nullptr) {
    if(!data||bytes<sizeof(header)+2||bytes>routing::kMaxPacketBytes||(bytes-sizeof(header))%2)return false;
    memcpy(&header,data,sizeof(header));if(header.magic!=routing::kMagic||header.version!=routing::kVersion||header.bytes!=bytes||header.itemCount>routing::kMaxItems||!header.sourcePid)return false;
    const wchar_t* value=reinterpret_cast<const wchar_t*>(static_cast<const BYTE*>(data)+sizeof(header));size_t remaining=(bytes-sizeof(header))/2;std::wstring parent;
    auto trim=[](std::wstring path){while(path.size()>3&&path.back()==L'\\')path.pop_back();return path;};
    for(DWORD i=0;i<=header.itemCount;i++){
        const wchar_t* end=std::find(value,value+remaining,L'\0');if(end==value+remaining)return false;size_t length=size_t(end-value);std::wstring path(value,length);if(!AbsolutePath(path))return false;
        if(i==0){parent=trim(path);if(folder)*folder=path;}
        else{size_t slash=path.find_last_of(L'\\');if(slash==std::wstring::npos)return false;std::wstring itemParent=trim(path.substr(0,slash+(slash==2?1:0)));if(_wcsicmp(parent.c_str(),itemParent.c_str()))return false;}
        remaining-=length+1;value=end+1;
    }
    return remaining==0;
}
bool AuthorizedSender(DWORD pid,ULONGLONG* creation=nullptr) {
    Handle process(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,FALSE,pid));if(!process||!SameOwner(process.get(),pid))return false;
    FILETIME created{},exited{},kernel{},user{};if(!GetProcessTimes(process.get(),&created,&exited,&kernel,&user))return false;
    if(creation)*creation=TimeNumber(created);
    {std::lock_guard<std::mutex> lock(gAuthMutex);auto found=gAuthorized.find(pid);if(found!=gAuthorized.end()&&found->second.created==TimeNumber(created)&&found->second.role!=1)return true;}
    wchar_t path[32768]{};DWORD length=DWORD(std::size(path));return QueryFullProcessImageNameW(process.get(),0,path,&length)&&!_wcsicmp(path,gExe.c_str());
}
void SaveStats() {
    routing::Status stats;{std::lock_guard<std::mutex> lock(gStatsMutex);stats=gStats;}
    std::ostringstream output;output<<"N360ROUTE1\n"<<GetCurrentProcessId()<<'\t'<<stats.received<<'\t'<<stats.completed<<'\t'<<stats.failed<<'\t'<<stats.declined<<'\t'<<stats.indeterminate<<'\t'<<stats.lastError<<'\t'<<stats.queued<<'\t'<<stats.lastId<<'\t'<<stats.reserved<<'\n';WriteText(kStatusState,output.str());
}
// The destination is pinned by canonical path and SHA-256 for each request.
// The file handle denies replacement while the process/window is selected and
// the copied packet is submitted. No private target code or DLL is injected.
struct Destination { std::wstring path,profileDir,profileToken;std::string hash;Handle image; };
DWORD HashHandle(HANDLE file,std::string& digest) {
    LARGE_INTEGER zero{};if(!SetFilePointerEx(file,zero,nullptr,FILE_BEGIN))return ErrorOr();
    BCRYPT_ALG_HANDLE algorithm=nullptr;BCRYPT_HASH_HANDLE hash=nullptr;
    bool good=BCryptOpenAlgorithmProvider(&algorithm,BCRYPT_SHA256_ALGORITHM,nullptr,0)>=0;
    DWORD bytes=0,returned=0;if(good)good=BCryptGetProperty(algorithm,BCRYPT_OBJECT_LENGTH,reinterpret_cast<PUCHAR>(&bytes),sizeof(bytes),&returned,0)>=0;
    std::vector<BYTE> object(bytes),buffer(65536);BYTE output[32]{};
    if(good)good=BCryptCreateHash(algorithm,&hash,object.data(),bytes,nullptr,0,0)>=0;
    while(good){DWORD count=0;if(!ReadFile(file,buffer.data(),DWORD(buffer.size()),&count,nullptr)){good=false;break;}if(!count)break;good=BCryptHashData(hash,buffer.data(),count,0)>=0;}
    if(good)good=BCryptFinishHash(hash,output,sizeof(output),0)>=0;
    if(hash)BCryptDestroyHash(hash);if(algorithm)BCryptCloseAlgorithmProvider(algorithm,0);
    if(!good)return ERROR_READ_FAULT;static const char hex[]="0123456789abcdef";digest.clear();for(BYTE value:output){digest.push_back(hex[value>>4]);digest.push_back(hex[value&15]);}return 0;
}
DWORD ReadDestination(Destination& destination) {
    wchar_t path[32768]{},hash[80]{};
    DWORD length=GetPrivateProfileStringW(L"Routing",L"TargetExe",L"",path,DWORD(std::size(path)),Join(kPackage).c_str());
    GetPrivateProfileStringW(L"Routing",L"TargetSHA256",L"",hash,DWORD(std::size(hash)),Join(kPackage).c_str());
    if(!length||length>=std::size(path)-1||!AbsolutePath(path)||wcslen(hash)!=64)return ERROR_BAD_CONFIGURATION;
    size_t size=wcslen(path);if(size<5||_wcsicmp(path+size-4,L".exe"))return ERROR_BAD_CONFIGURATION;
    destination.hash.clear();for(wchar_t c:hash){if(!c)break;if(c>=L'A'&&c<=L'F')c+=L'a'-L'A';if(!((c>=L'0'&&c<=L'9')||(c>=L'a'&&c<=L'f')))return ERROR_BAD_CONFIGURATION;destination.hash.push_back(char(c));}
    destination.image.reset(CreateFileW(path,GENERIC_READ,FILE_SHARE_READ,nullptr,OPEN_EXISTING,FILE_FLAG_SEQUENTIAL_SCAN,nullptr));
    if(destination.image.get()==INVALID_HANDLE_VALUE)return ErrorOr();
    wchar_t final[32768]{};DWORD finalLength=GetFinalPathNameByHandleW(destination.image.get(),final,DWORD(std::size(final)),FILE_NAME_NORMALIZED|VOLUME_NAME_DOS);
    if(!finalLength||finalLength>=std::size(final))return ErrorOr(ERROR_BAD_PATHNAME);
    const std::wstring prefix=std::wstring(2,wchar_t(92))+L"?"+wchar_t(92);
    destination.path=final;
    if(destination.path.rfind(prefix+L"UNC"+wchar_t(92),0)==0)destination.path=std::wstring(2,wchar_t(92))+destination.path.substr(8);
    else if(destination.path.rfind(prefix,0)==0)destination.path.erase(0,4);
    if(!AbsolutePath(destination.path))return ERROR_BAD_PATHNAME;
    destination.profileDir=RoutingIni(Join(kPackage),L"Routing",L"TargetProfileDir");
    std::wstring token=RoutingIni(Join(kPackage),L"Routing",L"TargetProfileToken");
    if(!destination.profileDir.empty()||!token.empty()){
        std::string tokenBytes;if(!RoutingLocalPath(destination.profileDir)||!RoutingHash(token,tokenBytes))return ERROR_BAD_CONFIGURATION;
        destination.profileToken.assign(tokenBytes.begin(),tokenBytes.end());
    }
    std::string actual;DWORD error=HashHandle(destination.image.get(),actual);return error?error:actual==destination.hash?0:ERROR_REVISION_MISMATCH;
}
bool DestinationProcess(DWORD pid,const Destination& destination,Target& target) {
    Handle process(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION|SYNCHRONIZE,FALSE,pid));
    if(!process||!SameOwner(process.get(),pid)||!IsX64Process(process.get())||WaitForSingleObject(process.get(),0)!=WAIT_TIMEOUT)return false;
    wchar_t path[32768]{};DWORD size=DWORD(std::size(path));FILETIME created{},exited{},kernel{},user{};
    if(!QueryFullProcessImageNameW(process.get(),0,path,&size)||_wcsicmp(path,destination.path.c_str())||!GetProcessTimes(process.get(),&created,&exited,&kernel,&user))return false;
    // A process predating the replacement image must not receive the new ABI.
    BY_HANDLE_FILE_INFORMATION image{};if(!GetFileInformationByHandle(destination.image.get(),&image)||TimeNumber(image.ftLastWriteTime)>TimeNumber(created))return false;
    target={pid,0,TimeNumber(created),path};return true;
}
struct WindowSearch { const Destination* destination;HWND window=nullptr;Target target; };
BOOL CALLBACK FindDestinationWindow(HWND window,LPARAM parameter) {
    auto& search=*reinterpret_cast<WindowSearch*>(parameter);wchar_t name[128]{};
    if(!GetClassNameW(window,name,DWORD(std::size(name)))||wcscmp(name,kTargetClass))return TRUE;
    DWORD pid=0;GetWindowThreadProcessId(window,&pid);Target target;
    if(!DestinationProcess(pid,*search.destination,target))return TRUE;
    if(!search.destination->profileToken.empty()){
        std::wstring property=L"ZSDesk.RoutingProfile."+search.destination->profileToken;
        if(reinterpret_cast<INT_PTR>(GetPropW(window,property.c_str()))!=1)return TRUE;
    }
    search.window=window;search.target=target;return FALSE;
}
HWND DestinationWindow(const Destination& destination,Target& target) {
    WindowSearch search{&destination,nullptr,{}};EnumWindows(FindDestinationWindow,reinterpret_cast<LPARAM>(&search));target=search.target;return search.window;
}
DWORD EnsureDestination(const Destination& destination,Target& target,HWND& window) {
    window=DestinationWindow(destination,target);if(window)return 0;
    if(gStop&&WaitForSingleObject(gStop,0)==WAIT_OBJECT_0)return ERROR_CANCELLED;
    std::wstring command=L"\""+destination.path+L"\"";if(!destination.profileDir.empty())command+=L" --profile-dir \""+destination.profileDir+L"\"";
    STARTUPINFOW startup{};startup.cb=sizeof(startup);startup.dwFlags=STARTF_USESHOWWINDOW;startup.wShowWindow=SW_HIDE;PROCESS_INFORMATION created{};
    if(!CreateProcessW(destination.path.c_str(),command.data(),nullptr,nullptr,FALSE,CREATE_NO_WINDOW,nullptr,nullptr,&startup,&created))return ErrorOr();
    Handle process(created.hProcess),thread(created.hThread);ULONGLONG deadline=GetTickCount64()+10000;
    do{
        window=DestinationWindow(destination,target);if(window)return 0;
        if(gStop){if(WaitForSingleObject(gStop,100)==WAIT_OBJECT_0)return ERROR_CANCELLED;}else Sleep(100);
    }while(GetTickCount64()<deadline);
    return ERROR_NOT_READY;
}
int DestinationShowCommand(bool visible,bool iconic,INT_PTR preferred) {
    if(iconic)return SW_RESTORE;
    if(!visible&&(preferred==SW_SHOWNORMAL||preferred==SW_SHOWMAXIMIZED))return int(preferred);
    return SW_SHOW;
}
DWORD Forward(const std::vector<BYTE>& packet,bool& uncertain,bool allowForeground=true) {
    uncertain=false;Destination destination;DWORD error=ReadDestination(destination);if(error)return error;
    Target target;HWND window=nullptr;error=EnsureDestination(destination,target,window);if(error)return error;
    if(gStop&&WaitForSingleObject(gStop,0)==WAIT_OBJECT_0)return ERROR_CANCELLED;
    Target checked;DWORD owner=0;GetWindowThreadProcessId(window,&owner);
    if(owner!=target.pid||!DestinationProcess(owner,destination,checked)||checked.created!=target.created)return ERROR_INVALID_WINDOW_HANDLE;
    COPYDATASTRUCT data{};data.dwData=routing::kCopyDataTag;data.cbData=DWORD(packet.size());data.lpData=const_cast<BYTE*>(packet.data());DWORD_PTR result=0;SetLastError(0);
    if(!SendMessageTimeoutW(window,WM_COPYDATA,0,reinterpret_cast<LPARAM>(&data),SMTO_ABORTIFHUNG|SMTO_BLOCK,1500,&result)){
        error=GetLastError();uncertain=error==ERROR_TIMEOUT||error==0;
        auto reason=uncertain?routing::Reason::Indeterminate:routing::Reason::AccessDenied;
        if(error==ERROR_ACCESS_DENIED){
            Handle process(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,FALSE,target.pid));DWORD own=0,other=0;
            if(process&&!routing::ProcessIntegrity(GetCurrentProcess(),own)&&!routing::ProcessIntegrity(process.get(),other)&&other>own)reason=routing::Reason::HigherIntegrity;
        }
        {std::lock_guard<std::mutex> lock(gStatsMutex);gStats.reserved=static_cast<DWORD>(reason);}
        return error?error:ERROR_TIMEOUT;
    }
    if(result!=1)return ERROR_NOT_SUPPORTED;
    {std::lock_guard<std::mutex> lock(gStatsMutex);gStats.reserved=0;}
    // Permission belongs to a process, not the target HWND. Once delegated,
    // the receiver must perform activation on its own UI thread. Asking this
    // worker to activate after delegation loses the inherited permission.
    if(allowForeground){
        DWORD currentOwner=0;GetWindowThreadProcessId(window,&currentOwner);Target current;
        if(currentOwner==target.pid&&DestinationProcess(currentOwner,destination,current)&&current.created==target.created){
            if(reinterpret_cast<INT_PTR>(GetPropW(window,routing::kActivationProperty))==1){
                AllowSetForegroundWindow(target.pid);
                COPYDATASTRUCT activation{routing::kActivateCopyDataTag,0,nullptr};DWORD_PTR shown=0;
                // A show failure never replays the already accepted navigation.
                SendMessageTimeoutW(window,WM_COPYDATA,0,reinterpret_cast<LPARAM>(&activation),SMTO_ABORTIFHUNG|SMTO_BLOCK,1500,&shown);
            }else{
                // Retain the broker's inherited permission for older receivers.
                const INT_PTR preferred=reinterpret_cast<INT_PTR>(GetPropW(window,L"ZSDesk.PreferredShow"));
                ShowWindowAsync(window,DestinationShowCommand(IsWindowVisible(window)!=FALSE,IsIconic(window)!=FALSE,preferred));
                SetForegroundWindow(window);
            }
        }
    }
    return 0;
}
void Worker() {
    ULONGLONG nextApply=0;
    while(WaitForSingleObject(gStop,0)!=WAIT_OBJECT_0){
        if(GetTickCount64()>=nextApply){ApplyCycle(false);nextApply=GetTickCount64()+2000;}
        std::vector<BYTE> packet;{std::lock_guard<std::mutex> lock(gQueueMutex);if(!gQueue.empty()){packet=std::move(gQueue.front());gQueue.pop_front();}}
        if(!packet.empty()){
            routing::PacketHeader header{};std::wstring folder;Packet(packet.data(),DWORD(packet.size()),header,&folder);bool uncertain=false;DWORD error=Forward(packet,uncertain);
            {std::lock_guard<std::mutex> lock(gStatsMutex);if(error){if(uncertain)++gStats.indeterminate;else ++gStats.failed;}else ++gStats.completed;gStats.lastError=error;gStats.lastId=header.requestId;gStats.lastSourceKind=header.sourceKind;gStats.lastItemCount=header.itemCount;if(gStats.queued)--gStats.queued;wcsncpy_s(gStats.lastFolder,folder.c_str(),_TRUNCATE);}SaveStats();continue;
        }
        HANDLE events[]={gStop,gWake};WaitForMultipleObjects(2,events,FALSE,200);
    }
    {std::lock_guard<std::mutex> lock(gQueueMutex);std::lock_guard<std::mutex> stats(gStatsMutex);gStats.failed+=DWORD(gQueue.size());gStats.queued=0;gQueue.clear();}SaveStats();
}
LRESULT CALLBACK WindowProcedure(HWND window,UINT message,WPARAM wparam,LPARAM lparam) {
    if(message==WM_COPYDATA){
        if(!gAccepting.load())return 0;
        auto* data=reinterpret_cast<const COPYDATASTRUCT*>(lparam);routing::PacketHeader header;ULONGLONG creation=0;
        if(!data||data->dwData!=routing::kCopyDataTag||!Packet(data->lpData,data->cbData,header)||!AuthorizedSender(header.sourcePid,&creation)){std::lock_guard<std::mutex> lock(gStatsMutex);++gStats.declined;return 0;}
        const RequestKey key{header.sourcePid,creation,header.requestId};
        try{
            std::lock_guard<std::mutex> lock(gQueueMutex);
            if(header.requestId&&gSeen.count(key))return 1;
            if(gQueue.size()>=16){std::lock_guard<std::mutex> stats(gStatsMutex);++gStats.declined;return 0;}
            bool recorded=false;
            try {
                if(header.requestId){gSeen.emplace(key,GetTickCount64());recorded=true;gSeenOrder.push_back(key);}
                const BYTE* bytes=static_cast<const BYTE*>(data->lpData);gQueue.emplace_back(bytes,bytes+data->cbData);
            } catch(...) {if(recorded){gSeen.erase(key);if(!gSeenOrder.empty()&&gSeenOrder.back()==key)gSeenOrder.pop_back();}throw;}
            while(gSeenOrder.size()>512){gSeen.erase(gSeenOrder.front());gSeenOrder.pop_front();}
            {std::lock_guard<std::mutex> stats(gStatsMutex);++gStats.received;++gStats.queued;}if(gWake)SetEvent(gWake);return 1;
        }catch(...){return 0;}
    }
    return DefWindowProcW(window,message,wparam,lparam);
}
DWORD Watch() {
    if(std::none_of(gSpecs.begin(),gSpecs.end(),[](const Spec& spec){return spec.enabled;}))return ERROR_NOT_READY;
    std::wstring path;std::string digest;DWORD error=Package(2,path,digest);if(error)return error;
    Destination destination;error=ReadDestination(destination);if(error)return error;destination.image.reset();
    Handle mutex(CreateMutexW(nullptr,FALSE,(gPrefix+L".Watch").c_str()));if(!mutex)return ErrorOr();DWORD acquired=WaitForSingleObject(mutex.get(),0);if(acquired!=WAIT_OBJECT_0&&acquired!=WAIT_ABANDONED)return ERROR_ALREADY_EXISTS;
    Handle stop(CreateEventW(nullptr,TRUE,FALSE,(gPrefix+L".Stop").c_str())),wake(CreateEventW(nullptr,FALSE,FALSE,nullptr));
    if(!stop||!wake){ReleaseMutex(mutex.get());return ErrorOr();}ResetEvent(stop.get());gStop=stop.get();gWake=wake.get();
    HRESULT com=CoInitializeEx(nullptr,COINIT_APARTMENTTHREADED);if(FAILED(com)){ReleaseMutex(mutex.get());return DWORD(com);}
    WNDCLASSEXW klass{};klass.cbSize=sizeof(klass);klass.lpfnWndProc=WindowProcedure;klass.hInstance=GetModuleHandleW(nullptr);klass.lpszClassName=kRouterClass;
    if(!RegisterClassExW(&klass)){CoUninitialize();ReleaseMutex(mutex.get());return ErrorOr();}
    HWND window=CreateWindowExW(0,kRouterClass,L"",0,0,0,0,0,HWND_MESSAGE,nullptr,klass.hInstance,nullptr);
    if(!window){error=ErrorOr();UnregisterClassW(kRouterClass,klass.hInstance);CoUninitialize();ReleaseMutex(mutex.get());return error;}
    WNDCLASSEXW legacy=klass;legacy.lpszClassName=kLegacyClass;ATOM legacyAtom=RegisterClassExW(&legacy);
    HWND legacyWindow=legacyAtom?CreateWindowExW(0,kLegacyClass,L"",0,0,0,0,0,HWND_MESSAGE,nullptr,legacy.hInstance,nullptr):nullptr;
    const std::wstring ownerProperty=gPrefix+L".Owner";
    for(HWND owned:{window,legacyWindow})if(owned){SetPropW(owned,L"ZSDesk.Routing.Protocol",reinterpret_cast<HANDLE>(INT_PTR(2)));SetPropW(owned,ownerProperty.c_str(),reinterpret_cast<HANDLE>(INT_PTR(1)));}
    gStats.installed=1;gStats.bridgeWindow=reinterpret_cast<ULONG_PTR>(window);gAccepting.store(true);
    FILETIME created{},exited{},kernel{},user{};GetProcessTimes(GetCurrentProcess(),&created,&exited,&kernel,&user);
    std::ostringstream state;state<<"N360WATCH1\n"<<GetCurrentProcessId()<<'\t'<<TimeNumber(created)<<'\t'<<gSession<<'\n';WriteText(kWatchState,state.str());SaveStats();
    std::thread worker(Worker);
    for(;;){DWORD waited=MsgWaitForMultipleObjects(1,&gStop,FALSE,INFINITE,QS_ALLINPUT);if(waited==WAIT_OBJECT_0)break;if(waited!=WAIT_OBJECT_0+1){error=ErrorOr();break;}MSG message{};while(PeekMessageW(&message,nullptr,0,0,PM_REMOVE)){if(message.message==WM_QUIT){SetEvent(gStop);break;}TranslateMessage(&message);DispatchMessageW(&message);}}
    gAccepting.store(false);SetEvent(gStop);SetEvent(gWake);worker.join();if(legacyWindow)DestroyWindow(legacyWindow);if(legacyAtom)UnregisterClassW(kLegacyClass,legacy.hInstance);DestroyWindow(window);UnregisterClassW(kRouterClass,klass.hInstance);gStop=nullptr;gWake=nullptr;CoUninitialize();ReleaseMutex(mutex.get());return error;
}
HWND OwnWatcherWindow(){
    const std::wstring property=gPrefix+L".Owner";
    for(const wchar_t* klass:{kRouterClass,kLegacyClass}){HWND window=nullptr;while((window=FindWindowExW(HWND_MESSAGE,window,klass,nullptr))!=nullptr){
        DWORD pid=0;GetWindowThreadProcessId(window,&pid);Handle process(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,FALSE,pid));if(!process||!SameOwner(process.get(),pid))continue;
        wchar_t path[32768]{};DWORD size=DWORD(std::size(path));if(!QueryFullProcessImageNameW(process.get(),0,path,&size)||_wcsicmp(path,gExe.c_str()))continue;
        INT_PTR protocol=reinterpret_cast<INT_PTR>(GetPropW(window,L"ZSDesk.Routing.Protocol"));
        if(protocol==2&&reinterpret_cast<INT_PTR>(GetPropW(window,property.c_str()))==1)return window;
        if(!protocol&&gRunName==L"Native360FileRevealRouting"&&!wcscmp(klass,kLegacyClass))return window;
    }}return nullptr;
}
DWORD StopWatcher() {
    Handle mutex(OpenMutexW(SYNCHRONIZE|MUTEX_MODIFY_STATE,FALSE,(gPrefix+L".Watch").c_str()));if(!mutex)return GetLastError()==ERROR_FILE_NOT_FOUND?0:ErrorOr();
    DWORD waited=WaitForSingleObject(mutex.get(),0);if(waited==WAIT_OBJECT_0||waited==WAIT_ABANDONED){ReleaseMutex(mutex.get());return 0;}
    if(!OwnWatcherWindow())return ERROR_ALREADY_EXISTS;
    Handle stop(OpenEventW(EVENT_MODIFY_STATE,FALSE,(gPrefix+L".Stop").c_str()));if(!stop||!SetEvent(stop.get()))return ErrorOr();waited=WaitForSingleObject(mutex.get(),45000);if(waited==WAIT_OBJECT_0||waited==WAIT_ABANDONED){ReleaseMutex(mutex.get());return 0;}return waited==WAIT_TIMEOUT?ERROR_BUSY:ErrorOr();
}
struct Value {bool present=false;DWORD type=0;std::vector<BYTE> bytes;};
bool Equal(const Value& a,const Value& b){return a.present==b.present&&(!a.present||(a.type==b.type&&a.bytes==b.bytes));}
Value ExpectedStartup(){Value value;value.present=true;value.type=REG_SZ;std::wstring line=L"\""+gExe+L"\" --watch";const BYTE* bytes=reinterpret_cast<const BYTE*>(line.c_str());value.bytes.assign(bytes,bytes+(line.size()+1)*2);return value;}
DWORD ReadStartup(Value& value) {
    HKEY key=nullptr;LSTATUS result=RegOpenKeyExW(HKEY_CURRENT_USER,kRunKey,0,KEY_QUERY_VALUE,&key);if(result==ERROR_FILE_NOT_FOUND)return 0;if(result)return DWORD(result);DWORD size=0;
    result=RegQueryValueExW(key,gRunName.c_str(),nullptr,&value.type,nullptr,&size);if(result==ERROR_FILE_NOT_FOUND){RegCloseKey(key);return 0;}if(result||size>1048576){RegCloseKey(key);return result?DWORD(result):ERROR_INVALID_DATA;}
    value.bytes.resize(size);result=RegQueryValueExW(key,gRunName.c_str(),nullptr,&value.type,value.bytes.data(),&size);RegCloseKey(key);if(result)return DWORD(result);value.bytes.resize(size);value.present=true;return 0;
}
DWORD WriteStartup(const Value& value) {
    HKEY key=nullptr;LSTATUS result=RegCreateKeyExW(HKEY_CURRENT_USER,kRunKey,0,nullptr,0,KEY_SET_VALUE,nullptr,&key,nullptr);if(result)return DWORD(result);
    result=value.present?RegSetValueExW(key,gRunName.c_str(),0,value.type,value.bytes.data(),DWORD(value.bytes.size())):RegDeleteValueW(key,gRunName.c_str());RegCloseKey(key);return result==ERROR_FILE_NOT_FOUND?0:DWORD(result);
}
struct BackupHeader {DWORD magic=0x31534252,version=1,active=0,originalPresent=0,originalType=0,originalBytes=0,expectedType=0,expectedBytes=0;};
struct Backup {bool active=false;Value original,expected;};
DWORD ReadBackup(Backup& backup) {
    std::vector<BYTE> bytes;DWORD error=ReadFileBytes(Join(kBackup),bytes);if(error)return error;if(bytes.size()<sizeof(BackupHeader))return ERROR_INVALID_DATA;BackupHeader header;memcpy(&header,bytes.data(),sizeof(header));
    if(header.magic!=0x31534252||header.version!=1||header.active>1||header.originalPresent>1||sizeof(header)+ULONGLONG(header.originalBytes)+header.expectedBytes!=bytes.size())return ERROR_INVALID_DATA;
    backup.active=header.active!=0;backup.original.present=header.originalPresent!=0;backup.original.type=header.originalType;backup.expected.present=true;backup.expected.type=header.expectedType;
    backup.expected.bytes.assign(bytes.begin()+sizeof(header),bytes.begin()+sizeof(header)+header.expectedBytes);backup.original.bytes.assign(bytes.begin()+sizeof(header)+header.expectedBytes,bytes.end());return 0;
}
DWORD SaveBackup(const Backup& backup) {
    BackupHeader header;header.active=backup.active;header.originalPresent=backup.original.present;header.originalType=backup.original.type;header.originalBytes=DWORD(backup.original.bytes.size());header.expectedType=backup.expected.type;header.expectedBytes=DWORD(backup.expected.bytes.size());
    std::vector<BYTE> bytes(sizeof(header));memcpy(bytes.data(),&header,sizeof(header));bytes.insert(bytes.end(),backup.expected.bytes.begin(),backup.expected.bytes.end());bytes.insert(bytes.end(),backup.original.bytes.begin(),backup.original.bytes.end());return WriteAtomic(Join(kBackup),bytes);
}
DWORD Startup(bool enable) {
    Value current;DWORD error=ReadStartup(current);if(error)return error;Backup backup;DWORD existing=ReadBackup(backup);Value expected=ExpectedStartup();
    if(enable){
        std::wstring path;std::string hash;error=Package(2,path,hash);if(error)return error;Destination destination;error=ReadDestination(destination);if(error)return error;
        if(!existing&&backup.active){if(!Equal(backup.expected,expected))return ERROR_ALREADY_EXISTS;if(Equal(current,expected))return 0;if(!Equal(current,backup.original))return ERROR_ALREADY_EXISTS;}
        else{if(existing&&existing!=ERROR_FILE_NOT_FOUND)return existing;backup.original=current;backup.expected=expected;backup.active=true;error=SaveBackup(backup);if(error)return error;}
        error=WriteStartup(expected);if(error)return error;Value after;error=ReadStartup(after);return error?error:Equal(after,expected)?0:ERROR_WRITE_FAULT;
    }
    if(existing==ERROR_FILE_NOT_FOUND&&!current.present)return 0;if(existing)return existing;
    if(!backup.active)return Equal(current,backup.original)?0:ERROR_ALREADY_EXISTS;
    if(!Equal(current,backup.expected)&&!Equal(current,backup.original))return ERROR_ALREADY_EXISTS;
    if(!Equal(current,backup.original)){error=WriteStartup(backup.original);if(error)return error;}Value after;error=ReadStartup(after);if(error||!Equal(after,backup.original))return error?error:ERROR_WRITE_FAULT;
    backup.active=false;return SaveBackup(backup);
}
DWORD RestoreTarget(const Target& target) {
    Handle process;DWORD error=OpenTarget(target,process);if(error)return error;std::vector<RemoteModule> modules;error=ReadModules(target.pid,modules);if(error)return error;
    BoundAdapter adapter;error=SelectAdapter(target,process.get(),modules,adapter,false);if(error)return error;uintptr_t module=adapter.base;if(!module)return 0;
    DWORD returned=0;error=CallScalar(process.get(),module,Export(target.role,1),returned);if(error)return error;if(returned!=0&&returned!=ERROR_BUSY)return returned;
    ULONGLONG deadline=GetTickCount64()+5000;do{routing::Status status;error=RemoteBuffer(process.get(),module,Export(target.role,2),status);if(error)return error;if(!status.installed&&!status.activeCalls&&!status.queued)return 0;Sleep(25);}while(GetTickCount64()<deadline);return ERROR_BUSY;
}
DWORD Restore() {
    DWORD first=StopWatcher();if(first)return first;OperationLock operation;if(operation.error)return operation.error;std::vector<Target> targets;DWORD error=Scan(targets);if(error)return error;
    std::stable_sort(targets.begin(),targets.end(),[](const Target& a,const Target& b){return a.role>b.role;});
    for(const auto& target:targets){error=RestoreTarget(target);Out(std::wstring(Specification(target.role)->name.c_str())+L" PID="+std::to_wstring(target.pid)+L" restore="+Hex(error));if(error&&!first)first=error;}
    error=Startup(false);if(error&&!first)first=error;return first;
}
bool WatcherRunning() {
    Handle mutex(OpenMutexW(SYNCHRONIZE|MUTEX_MODIFY_STATE,FALSE,(gPrefix+L".Watch").c_str()));if(!mutex)return false;DWORD waited=WaitForSingleObject(mutex.get(),0);if(waited==WAIT_OBJECT_0||waited==WAIT_ABANDONED)ReleaseMutex(mutex.get());return waited==WAIT_TIMEOUT&&OwnWatcherWindow();
}
DWORD Status() {
    DWORD integrity=0,elevated=0;const DWORD tokenError=routing::ProcessIntegrity(GetCurrentProcess(),integrity,&elevated);
    Out(L"controller integrity="+std::to_wstring(integrity)+L" elevated="+std::to_wstring(elevated)+L" token-error="+Hex(tokenError));
    Out(WatcherRunning()?L"routing watcher: running":L"routing watcher: stopped");Value startup;DWORD first=ReadStartup(startup);if(first)return first;Out(Equal(startup,ExpectedStartup())?L"routing startup: enabled":L"routing startup: not owned/enabled");
    OperationLock operation;if(operation.error)return operation.error;std::vector<Target> targets;DWORD error=Scan(targets);if(error)return error;
    for(const auto& target:targets){routing::Status status;bool loaded=false;error=TargetStatus(target,status,loaded);std::wstring row=std::wstring(Specification(target.role)->name.c_str())+L" PID="+std::to_wstring(target.pid);if(error)row+=L" status-error="+Hex(error);else if(!loaded)row+=L" adapter=not-loaded";else row+=L" role="+std::to_wstring(status.role)+L" installed="+std::to_wstring(status.installed)+L" active="+std::to_wstring(status.activeCalls)+L" received="+std::to_wstring(status.received)+L" completed="+std::to_wstring(status.completed)+L" failed="+std::to_wstring(status.failed)+L" declined="+std::to_wstring(status.declined)+L" uncertain="+std::to_wstring(status.indeterminate)+L" queued="+std::to_wstring(status.queued)+L" last="+Hex(status.lastError)+L" reason="+std::to_wstring(status.reserved)+L":"+routing::ReasonName(static_cast<routing::Reason>(status.reserved))+L" items="+std::to_wstring(status.lastItemCount)+L" folder="+status.lastFolder;
        Handle process(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,FALSE,target.pid));DWORD sourceIntegrity=0;
        if(process&&!routing::ProcessIntegrity(process.get(),sourceIntegrity))row+=L" integrity="+std::to_wstring(sourceIntegrity);
        Out(row);if(error&&!first)first=error;}
    for(const wchar_t* file:{kStatusState,L"routing-launch.current.tsv"}){
        std::vector<BYTE> state;if(!ReadFileBytes(Join(file),state)){std::string text(state.begin(),state.end());int n=MultiByteToWideChar(CP_UTF8,0,text.data(),int(text.size()),nullptr,0);std::wstring wide(size_t(n),L'\0');MultiByteToWideChar(CP_UTF8,0,text.data(),int(text.size()),wide.data(),n);Out(wide);}
    }return first;
}
DWORD Reveal(const wchar_t* item) {
    std::wstring full=item;if(!AbsolutePath(full))return ERROR_BAD_PATHNAME;DWORD attributes=GetFileAttributesW(full.c_str());if(attributes==INVALID_FILE_ATTRIBUTES)return ErrorOr();
    std::wstring folder=full;DWORD count=0;if(!(attributes&FILE_ATTRIBUTE_DIRECTORY)){size_t slash=full.find_last_of(L'\\');if(slash==std::wstring::npos)return ERROR_BAD_PATHNAME;folder=full.substr(0,slash+(slash==2?1:0));count=1;}
    routing::PacketHeader header;header.itemCount=count;header.sourcePid=GetCurrentProcessId();header.sourceKind=4;header.requestId=Now()^GetCurrentProcessId();header.bytes=DWORD(sizeof(header)+(folder.size()+1+(count?full.size()+1:0))*2);std::vector<BYTE> packet(header.bytes);memcpy(packet.data(),&header,sizeof(header));wchar_t* strings=reinterpret_cast<wchar_t*>(packet.data()+sizeof(header));memcpy(strings,folder.c_str(),(folder.size()+1)*2);if(count)memcpy(strings+folder.size()+1,full.c_str(),(full.size()+1)*2);
    HWND window=OwnWatcherWindow();if(!window)return ERROR_NOT_FOUND;DWORD owner=0;GetWindowThreadProcessId(window,&owner);Handle process(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,FALSE,owner));if(!process||!SameOwner(process.get(),owner))return ERROR_ACCESS_DENIED;
    AllowSetForegroundWindow(owner);
    COPYDATASTRUCT data{};data.dwData=routing::kCopyDataTag;data.cbData=DWORD(packet.size());data.lpData=packet.data();DWORD_PTR reply=0;SetLastError(0);if(!SendMessageTimeoutW(window,WM_COPYDATA,0,reinterpret_cast<LPARAM>(&data),SMTO_ABORTIFHUNG|SMTO_BLOCK,1500,&reply))return ErrorOr(ERROR_TIMEOUT);return reply==1?0:ERROR_NOT_SUPPORTED;
}
void AddDiscovery(std::map<std::wstring,Compatibility>& found,const std::wstring& path,const std::wstring& caller=L""){
    if(path.empty()||found.size()>=128)return;std::wstring normalized=path;
    if(normalized.front()==L'"'){size_t end=normalized.find(L'"',1);if(end==std::wstring::npos)return;normalized=normalized.substr(1,end-1);}
    if(normalized.size()<4||_wcsicmp(normalized.c_str()+normalized.size()-4,L".exe"))return;
    std::wstring key=normalized;std::transform(key.begin(),key.end(),key.begin(),[](wchar_t c){return wchar_t(towlower(c));});if(found.count(key))return;
    auto value=InspectCompatibility(normalized,caller);if(value.supported)found.emplace(std::move(key),std::move(value));
}
DWORD Discover(){
    std::map<std::wstring,Compatibility> found;Handle snapshot(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS,0));
    if(snapshot.get()!=INVALID_HANDLE_VALUE){PROCESSENTRY32W entry{};entry.dwSize=sizeof(entry);if(Process32FirstW(snapshot.get(),&entry))do{
        Handle process(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,FALSE,entry.th32ProcessID));if(!process||!SameOwner(process.get(),entry.th32ProcessID)||!IsMain(process.get()))continue;
        wchar_t path[32768]{};DWORD size=DWORD(std::size(path));if(QueryFullProcessImageNameW(process.get(),0,path,&size)&&_wcsicmp(path,gExe.c_str())){
            std::wstring caller;std::vector<RemoteModule> modules;if(!ReadModules(entry.th32ProcessID,modules))for(const auto& module:modules){const auto* name=BaseName(module.path.c_str());if(!_wcsicmp(name,L"chrome.dll")||!_wcsicmp(name,L"Weixin.dll")){caller=module.path;break;}}
            AddDiscovery(found,path,caller);
        }
    }while(Process32NextW(snapshot.get(),&entry));}
    for(HKEY hive:{HKEY_CURRENT_USER,HKEY_LOCAL_MACHINE}){
        HKEY apps=nullptr;if(RegOpenKeyExW(hive,L"Software\\Microsoft\\Windows\\CurrentVersion\\App Paths",0,KEY_READ,&apps)!=ERROR_SUCCESS)continue;
        for(DWORD i=0;i<512;++i){wchar_t name[512]{};DWORD chars=DWORD(std::size(name));if(RegEnumKeyExW(apps,i,name,&chars,nullptr,nullptr,nullptr,nullptr)!=ERROR_SUCCESS)break;
            HKEY app=nullptr;if(RegOpenKeyExW(apps,name,0,KEY_READ,&app)!=ERROR_SUCCESS)continue;wchar_t value[32768]{};DWORD type=0,bytes=sizeof(value);
            if(RegQueryValueExW(app,nullptr,nullptr,&type,reinterpret_cast<BYTE*>(value),&bytes)==ERROR_SUCCESS&&(type==REG_SZ||type==REG_EXPAND_SZ)&&bytes<sizeof(value)){
                wchar_t expanded[32768]{};if(type==REG_EXPAND_SZ&&ExpandEnvironmentStringsW(value,expanded,DWORD(std::size(expanded)))>0)AddDiscovery(found,expanded);else AddDiscovery(found,value);
            }RegCloseKey(app);
        }RegCloseKey(apps);
    }
    for(HKEY hive:{HKEY_CURRENT_USER,HKEY_LOCAL_MACHINE})for(const wchar_t* branch:{L"Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall",L"Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall"}){
        HKEY uninstall=nullptr;if(RegOpenKeyExW(hive,branch,0,KEY_READ,&uninstall)!=ERROR_SUCCESS)continue;
        for(DWORD i=0;i<512&&found.size()<128;++i){wchar_t name[512]{};DWORD chars=DWORD(std::size(name));if(RegEnumKeyExW(uninstall,i,name,&chars,nullptr,nullptr,nullptr,nullptr)!=ERROR_SUCCESS)break;
            HKEY app=nullptr;if(RegOpenKeyExW(uninstall,name,0,KEY_READ,&app)!=ERROR_SUCCESS)continue;wchar_t value[32768]{};DWORD type=0,bytes=sizeof(value);
            if(RegQueryValueExW(app,L"DisplayIcon",nullptr,&type,reinterpret_cast<BYTE*>(value),&bytes)==ERROR_SUCCESS&&type==REG_SZ&&bytes<sizeof(value)){
                std::wstring icon=value;if(!icon.empty()&&icon.front()!=L'"'){size_t comma=icon.find_last_of(L',');if(comma!=std::wstring::npos)icon.resize(comma);}AddDiscovery(found,icon);
            }RegCloseKey(app);
        }RegCloseKey(uninstall);
    }
    std::wstring json=L"{\"schema\":\"ZSDesk.RoutingDiscovery.v1\",\"applications\":[";bool first=true;for(const auto& item:found){if(!first)json+=L",";first=false;json+=CompatibilityJson(item.second);}json+=L"]}";Out(json);return 0;
}
DWORD Inspect(const wchar_t* path,const wchar_t* caller=nullptr){auto value=InspectCompatibility(path,caller?caller:L"");Out(CompatibilityJson(value));return value.supported?0:ERROR_NOT_SUPPORTED;}
DWORD SelfTest() {
    routing::PacketHeader header;header.sourcePid=1;std::wstring folder=L"E:\\fixture",item=L"E:\\fixture\\alpha, space.txt";header.itemCount=1;header.bytes=DWORD(sizeof(header)+(folder.size()+item.size()+2)*2);std::vector<BYTE> bytes(header.bytes);memcpy(bytes.data(),&header,sizeof(header));memcpy(bytes.data()+sizeof(header),folder.c_str(),(folder.size()+1)*2);memcpy(bytes.data()+sizeof(header)+(folder.size()+1)*2,item.c_str(),(item.size()+1)*2);routing::PacketHeader parsed;
    if(!Packet(bytes.data(),DWORD(bytes.size()),parsed))return 1;bytes.back()=1;if(Packet(bytes.data(),DWORD(bytes.size()),parsed))return 2;bytes.back()=0;if(Packet(bytes.data(),DWORD(bytes.size()-1),parsed))return 3;
    if(AbsolutePath(L"relative\\path")||AbsolutePath(L"\\\\?\\E:\\file")||AbsolutePath(L"E:\\x\"y")||!AbsolutePath(L"\\\\server\\share\\x")||!AbsolutePath(L"E:\\"))return 4;Out(L"self-test: 8 checks passed");return 0;
}
DWORD ForwardTest(const wchar_t* path) {
    std::vector<BYTE> packet;DWORD error=ReadFileBytes(path,packet);if(error)return error;routing::PacketHeader header;
    if(!Packet(packet.data(),DWORD(packet.size()),header))return ERROR_INVALID_DATA;
    bool uncertain=false;error=Forward(packet,uncertain,false);
    Out(L"forward-test result="+Hex(error)+L" uncertain="+std::to_wstring(uncertain)+L" activation=disabled");return error;
}
bool UnsignedArgument(const wchar_t* text,ULONGLONG& value){
    if(!text||!*text)return false;value=0;
    for(const wchar_t* at=text;*at;++at){if(*at<L'0'||*at>L'9'||value>(ULLONG_MAX-ULONGLONG(*at-L'0'))/10)return false;value=value*10+ULONGLONG(*at-L'0');}
    return true;
}
DWORD ParentProcess(){
    Handle snapshot(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS,0));if(snapshot.get()==INVALID_HANDLE_VALUE)return 0;
    PROCESSENTRY32W process{};process.dwSize=sizeof(process);
    if(Process32FirstW(snapshot.get(),&process))do{if(process.th32ProcessID==GetCurrentProcessId())return process.th32ParentProcessID;}while(Process32NextW(snapshot.get(),&process));
    return 0;
}
DWORD RouteLaunch(int argc,wchar_t** argv){
    if(argc<6||argc>int(routing::kMaxItems)+6)return ERROR_INVALID_PARAMETER;
    ULONGLONG source=0,kind=0,id=0;
    if(!UnsignedArgument(argv[2],source)||!source||source>MAXDWORD||!UnsignedArgument(argv[3],kind)||kind<10||kind>12||!UnsignedArgument(argv[4],id)||!id||ParentProcess()!=source)return ERROR_INVALID_PARAMETER;
    Handle parent(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,FALSE,DWORD(source)));if(!parent)return ErrorOr();
    Target origin;const Spec* binding=nullptr;
    for(const auto& spec:gSpecs)if(spec.enabled&&!Identify(parent.get(),DWORD(source),spec.role,origin)){binding=&spec;break;}
    if(!binding)return ERROR_ACCESS_DENIED;
    std::vector<RemoteModule> modules;DWORD error=ReadModules(DWORD(source),modules);if(error)return error;
    error=VerifyTarget(origin,modules);if(error)return error;
    shell_route::RouteRequest request;
    if(!shell_route::NormalizeAbsolute(argv[5],request.folder))return ERROR_BAD_PATHNAME;
    for(int index=6;index<argc;++index)request.items.emplace_back(argv[index]);
    if(!request.items.empty()){
        shell_route::RouteRequest checked;if(!shell_route::Selection(request.folder,request.items,0,checked))return ERROR_INVALID_DATA;request=std::move(checked);
    }
    const DWORD attributes=GetFileAttributesW(request.folder.c_str());if(attributes==INVALID_FILE_ATTRIBUTES||!(attributes&FILE_ATTRIBUTE_DIRECTORY))return ERROR_PATH_NOT_FOUND;
    for(const auto& item:request.items)if(GetFileAttributesW(item.c_str())==INVALID_FILE_ATTRIBUTES)return ERROR_FILE_NOT_FOUND;
    size_t characters=request.folder.size()+1;for(const auto& item:request.items)characters+=item.size()+1;
    if(characters>(routing::kMaxPacketBytes-sizeof(routing::PacketHeader))/sizeof(wchar_t))return ERROR_BUFFER_OVERFLOW;
    routing::PacketHeader header;header.sourcePid=DWORD(source);header.sourceKind=DWORD(kind);header.requestId=id;header.itemCount=DWORD(request.items.size());header.bytes=DWORD(sizeof(header)+characters*sizeof(wchar_t));
    std::vector<BYTE> packet(header.bytes);memcpy(packet.data(),&header,sizeof(header));size_t offset=sizeof(header);
    auto append=[&](const std::wstring& value){const size_t bytes=(value.size()+1)*sizeof(wchar_t);memcpy(packet.data()+offset,value.c_str(),bytes);offset+=bytes;};append(request.folder);for(const auto& item:request.items)append(item);
    bool uncertain=false;error=Forward(packet,uncertain);
    return error;
}
void RecordLaunchResult(int argc,wchar_t** argv,DWORD error){
    // Creation succeeded before this method ran. Failures are exposed through
    // the real process exit code and one bounded diagnostic record, never replayed.
    ULONGLONG source=0,kind=0,id=0;if(argc>=5){UnsignedArgument(argv[2],source);UnsignedArgument(argv[3],kind);UnsignedArgument(argv[4],id);}
    DWORD reason=0;{std::lock_guard<std::mutex> lock(gStatsMutex);reason=gStats.reserved;}
    Handle mutex(CreateMutexW(nullptr,FALSE,(gPrefix+L".LaunchStatus").c_str()));
    if(mutex){const DWORD wait=WaitForSingleObject(mutex.get(),200);if(wait==WAIT_OBJECT_0||wait==WAIT_ABANDONED){
        std::ostringstream record;record<<"ZSDesk.RouteLaunch.v1\n"<<source<<'\t'<<kind<<'\t'<<id<<'\t'<<error<<'\t'<<(reason==static_cast<DWORD>(routing::Reason::Indeterminate))<<'\t'<<reason<<'\n';WriteText(L"routing-launch.current.tsv",record.str());ReleaseMutex(mutex.get());
    }}
}
DWORD UpgradeCheck(const wchar_t* clients,const wchar_t* previous){
    std::vector<Spec> bindings;DWORD error=ReadClientBindings(clients,bindings);if(error)return error;
    if(previous){std::vector<Spec> old;error=ReadClientBindings(previous,old);if(error)return error;
        for(auto& entry:old)if(entry.enabled&&std::none_of(bindings.begin(),bindings.end(),[&](const Spec& current){return !_wcsicmp(current.path.c_str(),entry.path.c_str());}))bindings.push_back(std::move(entry));
    }
    std::wstring json=L"{\"schema\":\"ZSDesk.RoutingUpgrade.v1\",\"processes\":[";bool first=true;unsigned blocked=0;
    Handle snapshot(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS,0));if(snapshot.get()==INVALID_HANDLE_VALUE)return ErrorOr();
    PROCESSENTRY32W entry{};entry.dwSize=sizeof(entry);if(!Process32FirstW(snapshot.get(),&entry))return ErrorOr();
    do{for(const auto& binding:bindings){
        if(!binding.enabled||_wcsicmp(entry.szExeFile,binding.name.c_str()))continue;
        Handle process(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,FALSE,entry.th32ProcessID));if(!process)return ErrorOr();
        wchar_t path[32768]{};DWORD length=DWORD(std::size(path));if(!QueryFullProcessImageNameW(process.get(),0,path,&length))return ErrorOr();
        if(_wcsicmp(path,binding.path.c_str()))continue;
        std::vector<RemoteModule> modules;error=ReadModules(entry.th32ProcessID,modules);if(error)return error;
        for(const auto& module:modules)if(!_wcsicmp(BaseName(module.path.c_str()),L"ZSDeskRevealRouter.dll")||!_wcsicmp(BaseName(module.path.c_str()),L"FileRevealRouter.dll")){
            if(!first)json+=L",";first=false;++blocked;
            json+=L"{\"pid\":"+std::to_wstring(entry.th32ProcessID)+L",\"application\":"+RoutingJson(binding.name)+L",\"module\":"+RoutingJson(module.path)+L"}";break;
        }
        break;
    }}while(Process32NextW(snapshot.get(),&entry));
    json+=L"],\"restart_required\":"+std::wstring(blocked?L"true":L"false")+L"}";Out(json);return blocked?ERROR_RESTART_APPLICATION:0;
}
} // namespace

#ifndef ZSDESK_ROUTING_NO_MAIN
int wmain(int argc,wchar_t** argv) {
    if(argc<2){std::fwprintf(stderr,L"ShellRoutingControl --apply-once | --status | --watch | --stop | --enable-startup | --disable-startup | --restore | --reveal path | --forward-test packet | --self-test\n");return ERROR_INVALID_PARAMETER;}
    const bool watching=!wcscmp(argv[1],L"--watch");
    gQuiet=watching||!wcscmp(argv[1],L"--route-launch");DWORD error=Initialize();if(error){Out(L"initialization: "+Hex(error));return int(error);}
    try {
        if(argc==2&&!wcscmp(argv[1],L"--discover"))error=Discover();
        else if((argc==3||argc==4)&&!wcscmp(argv[1],L"--inspect"))error=Inspect(argv[2],argc==4?argv[3]:nullptr);
        else if(argc==2&&!wcscmp(argv[1],L"--apply-once"))error=ApplyCycle(true);
        else if(argc==2&&!wcscmp(argv[1],L"--status"))error=Status();
        else if(argc==2&&!wcscmp(argv[1],L"--watch"))error=Watch();
        else if(argc==2&&!wcscmp(argv[1],L"--stop"))error=StopWatcher();
        else if(argc==2&&!wcscmp(argv[1],L"--restore"))error=Restore();
        else if(argc==2&&(!wcscmp(argv[1],L"--enable-startup")||!wcscmp(argv[1],L"--disable-startup"))){OperationLock operation;error=operation.error;if(!error)error=Startup(!wcscmp(argv[1],L"--enable-startup"));}
        else if(argc==3&&!wcscmp(argv[1],L"--reveal"))error=Reveal(argv[2]);
        else if(argc==3&&!wcscmp(argv[1],L"--forward-test"))error=ForwardTest(argv[2]);
        else if(argc>=6&&!wcscmp(argv[1],L"--route-launch")){error=RouteLaunch(argc,argv);RecordLaunchResult(argc,argv,error);}
        else if((argc==3||argc==4)&&!wcscmp(argv[1],L"--upgrade-check"))error=UpgradeCheck(argv[2],argc==4?argv[3]:nullptr);
        else if(argc==2&&!wcscmp(argv[1],L"--self-test"))error=SelfTest();
        else error=ERROR_INVALID_PARAMETER;
    } catch(...) {error=ERROR_UNHANDLED_EXCEPTION;}
    if(error&&!(watching&&error==ERROR_ALREADY_EXISTS))Out(L"result="+Hex(error));return int(watching&&error==ERROR_ALREADY_EXISTS?0:error);
}
#endif
