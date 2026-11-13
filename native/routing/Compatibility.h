#pragma once
#include "support/RemoteAdapter.h"
#include <algorithm>
#include <string_view>
#include <set>
#include <cctype>

struct Compatibility { bool supported=false;DWORD profile=0;std::wstring executable,caller,label,reason; };
class PeImports {
    Handle file;ULONGLONG length=0;IMAGE_NT_HEADERS64 nt{};std::vector<IMAGE_SECTION_HEADER> sections;
    bool Read(ULONGLONG offset,void* output,DWORD size){if(offset>length||size>length-offset)return false;LARGE_INTEGER at{};at.QuadPart=offset;DWORD read=0;return SetFilePointerEx(file.get(),at,nullptr,FILE_BEGIN)&&ReadFile(file.get(),output,size,&read,nullptr)&&read==size;}
    bool Offset(DWORD rva,ULONGLONG& offset){
        if(rva<nt.OptionalHeader.SizeOfHeaders){offset=rva;return offset<length;}
        for(const auto& section:sections)if(rva>=section.VirtualAddress&&ULONGLONG(rva)-section.VirtualAddress<section.SizeOfRawData){offset=ULONGLONG(section.PointerToRawData)+rva-section.VirtualAddress;return offset<length;}return false;
    }
    bool Text(DWORD rva,std::string& text){ULONGLONG offset=0;if(!Offset(rva,offset))return false;text.clear();for(unsigned i=0;i<512;++i){char c=0;if(!Read(offset+i,&c,1))return false;if(!c)return true;text.push_back(c);}return false;}
    bool Names(DWORD libraryRva,DWORD thunkRva){
        std::string library;if(!Text(libraryRva,library))return false;
        std::transform(library.begin(),library.end(),library.begin(),[](unsigned char c){return char(tolower(c));});
        const bool shell=library=="shell32.dll";
        const bool process=library=="kernel32.dll"||library=="kernelbase.dll"||library.rfind("api-ms-win-core-processthreads-",0)==0;
        if(!shell&&!process)return false;
        ULONGLONG thunk=0;if(!Offset(thunkRva,thunk))return false;
        for(unsigned j=0;j<4096;++j){ULONGLONG value=0;if(!Read(thunk+ULONGLONG(j)*8,&value,8)||!value)break;if(value&IMAGE_ORDINAL_FLAG64)continue;if(value>MAXDWORD-2)return false;
            std::string name;if(!Text(DWORD(value)+2,name))return false;
            if(shell&&(name=="SHOpenFolderAndSelectItems"||name=="ShellExecuteW"||name=="ShellExecuteExW"))return true;
            if(process&&(name=="CreateProcessW"||name=="CreateProcessA"||name=="WinExec"))return true;
        }return false;
    }
public:
    bool Open(const std::wstring& path){
        file.reset(CreateFileW(path.c_str(),GENERIC_READ,FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,nullptr,OPEN_EXISTING,FILE_ATTRIBUTE_NORMAL,nullptr));if(file.get()==INVALID_HANDLE_VALUE)return false;
        LARGE_INTEGER size{};if(!GetFileSizeEx(file.get(),&size)||size.QuadPart<sizeof(IMAGE_DOS_HEADER)||size.QuadPart>2LL*1024*1024*1024)return false;length=ULONGLONG(size.QuadPart);
        IMAGE_DOS_HEADER dos{};if(!Read(0,&dos,sizeof(dos))||dos.e_magic!=IMAGE_DOS_SIGNATURE||dos.e_lfanew<=0||dos.e_lfanew>1048576)return false;
        if(!Read(DWORD(dos.e_lfanew),&nt,sizeof(nt))||nt.Signature!=IMAGE_NT_SIGNATURE||nt.FileHeader.Machine!=IMAGE_FILE_MACHINE_AMD64||nt.OptionalHeader.Magic!=IMAGE_NT_OPTIONAL_HDR64_MAGIC||nt.FileHeader.NumberOfSections>96||nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_COM_DESCRIPTOR].VirtualAddress)return false;
        sections.resize(nt.FileHeader.NumberOfSections);return Read(ULONGLONG(dos.e_lfanew)+4+sizeof(IMAGE_FILE_HEADER)+nt.FileHeader.SizeOfOptionalHeader,sections.data(),DWORD(sections.size()*sizeof(IMAGE_SECTION_HEADER)));
    }
    bool StandardRevealImports(){
        auto directory=nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];ULONGLONG at=0;
        if(directory.VirtualAddress&&directory.Size<=1048576&&Offset(directory.VirtualAddress,at)){
            for(unsigned i=0;i<512;++i){IMAGE_IMPORT_DESCRIPTOR descriptor{};if(!Read(at+ULONGLONG(i)*sizeof(descriptor),&descriptor,sizeof(descriptor))||!descriptor.Name)break;
                if(Names(descriptor.Name,descriptor.OriginalFirstThunk?descriptor.OriginalFirstThunk:descriptor.FirstThunk))return true;
            }
        }
        // Chromium and WeChat commonly delay-load these documented Shell APIs.
        struct DelayDescriptor{DWORD attributes,name,module,iat,names,bound,unload,timestamp;};
        auto delayed=nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT];
        if(delayed.VirtualAddress&&delayed.Size<=1048576&&Offset(delayed.VirtualAddress,at)){
            for(unsigned i=0;i<512;++i){DelayDescriptor descriptor{};if(!Read(at+ULONGLONG(i)*sizeof(descriptor),&descriptor,sizeof(descriptor))||!descriptor.name)break;
                if((descriptor.attributes&1)&&Names(descriptor.name,descriptor.names?descriptor.names:descriptor.iat))return true;
            }
        }return false;
    }
};
inline bool RoutingSystemPath(const std::wstring& path){
    wchar_t windows[32768]{};DWORD n=GetWindowsDirectoryW(windows,DWORD(std::size(windows)));if(!n||n>=std::size(windows))return true;
    std::wstring prefix=std::wstring(windows)+wchar_t(92);return !_wcsnicmp(path.c_str(),prefix.c_str(),prefix.size());
}
inline bool RoutingProtectedProcess(const std::wstring& path){
    for(const wchar_t* name:{L"explorer.exe",L"winlogon.exe",L"dwm.exe",L"svchost.exe",L"services.exe",L"lsass.exe",L"smss.exe",L"csrss.exe",L"wininit.exe"})
        if(!_wcsicmp(BaseName(path.c_str()),name))return true;
    return RoutingSystemPath(path);
}
inline bool RoutingContainedModule(const std::wstring& executable,const std::wstring& module){
    wchar_t exe[32768]{},dll[32768]{};
    DWORD a=GetFullPathNameW(executable.c_str(),DWORD(std::size(exe)),exe,nullptr),b=GetFullPathNameW(module.c_str(),DWORD(std::size(dll)),dll,nullptr);
    if(!a||!b||a>=std::size(exe)||b>=std::size(dll)||RoutingSystemPath(dll))return false;
    std::wstring root=exe;const size_t slash=root.find_last_of(L"\\/");if(slash==std::wstring::npos)return false;root.resize(slash+1);
    return b>root.size()&&!_wcsnicmp(dll,root.c_str(),root.size())&&b>=4&&!_wcsicmp(dll+b-4,L".dll");
}
inline Compatibility InspectCompatibility(const std::wstring& executable,const std::wstring& requestedCaller=L""){
    Compatibility result;result.executable=executable;result.label=BaseName(executable.c_str());
    if(executable.size()<4||executable.size()>=2048||executable[1]!=L':'||RoutingProtectedProcess(executable)){result.reason=L"Only selected desktop applications are supported; Explorer and system processes are excluded";return result;}
    std::wstring root=executable.substr(0,3);if(GetDriveTypeW(root.c_str())==DRIVE_REMOTE){result.reason=L"Network executable bindings are not supported";return result;}
    PeImports image;if(!image.Open(executable)){result.reason=L"A native x64 executable is required";return result;}
    std::wstring parent=executable.substr(0,executable.find_last_of(L"\\/"));std::vector<std::pair<std::wstring,DWORD>> candidates;
    if(!requestedCaller.empty()){
        if(_wcsicmp(requestedCaller.c_str(),executable.c_str())&&!RoutingContainedModule(executable,requestedCaller)){
            result.reason=L"A caller DLL must be explicitly bound inside the selected application's directory";return result;
        }
        DWORD profile=!_wcsicmp(BaseName(requestedCaller.c_str()),L"chrome.dll")?2:!_wcsicmp(BaseName(requestedCaller.c_str()),L"Weixin.dll")?3:1;
        if(profile==1&&_wcsicmp(requestedCaller.c_str(),executable.c_str())){
            if(!RoutingContainedModule(executable,requestedCaller)){result.reason=L"An explicitly selected helper DLL must be inside this application's directory";return result;}
            profile=4;
        }candidates.push_back({requestedCaller,profile});
    }else{
        candidates.push_back({parent+L"\\chrome.dll",2});
        if(!_wcsicmp(BaseName(executable.c_str()),L"Weixin.exe")){
            candidates.push_back({parent+L"\\Weixin.dll",3});WIN32_FIND_DATAW entry{};HANDLE find=FindFirstFileW((parent+L"\\*").c_str(),&entry);
            if(find!=INVALID_HANDLE_VALUE){unsigned examined=0;do{if(++examined>128)break;if((entry.dwFileAttributes&FILE_ATTRIBUTE_DIRECTORY)&&entry.cFileName[0]!=L'.')candidates.push_back({parent+L"\\"+entry.cFileName+L"\\Weixin.dll",3});}while(FindNextFileW(find,&entry));FindClose(find);}
        }
        WIN32_FIND_DATAW version{};HANDLE versions=FindFirstFileW((parent+L"\\*").c_str(),&version);
        if(versions!=INVALID_HANDLE_VALUE){unsigned examined=0;do{if(++examined>128)break;if((version.dwFileAttributes&FILE_ATTRIBUTE_DIRECTORY)&&iswdigit(version.cFileName[0]))candidates.push_back({parent+L"\\"+version.cFileName+L"\\chrome.dll",2});}while(FindNextFileW(versions,&version));FindClose(versions);}
        candidates.push_back({executable,1});
    }
    for(const auto& candidate:candidates){PeImports module;if(module.Open(candidate.first)&&module.StandardRevealImports()){result.supported=true;result.profile=candidate.second;result.caller=candidate.first;result.reason=L"Documented Shell or process-launch imports found; only supported filesystem navigation is routed";return result;}}
    result.reason=L"No supported imported Shell/process-launch entry was found; custom browsers and file dialogs are not Explorer navigation";return result;
}
inline std::wstring RoutingJson(const std::wstring& value){
    std::wstring result=L"\"";for(wchar_t c:value){if(c==L'"'||c==wchar_t(92)){result+=wchar_t(92);result+=c;}else if(c<32){wchar_t text[7]{};swprintf_s(text,L"\\u%04x",unsigned(c));result+=text;}else result+=c;}return result+L"\"";
}
inline std::wstring CompatibilityJson(const Compatibility& value){return L"{\"executable\":"+RoutingJson(value.executable)+L",\"caller_module\":"+RoutingJson(value.caller)+L",\"label\":"+RoutingJson(value.label)+L",\"supported\":"+(value.supported?L"true":L"false")+L",\"profile\":"+std::to_wstring(value.profile)+L",\"reason\":"+RoutingJson(value.reason)+L"}";}
