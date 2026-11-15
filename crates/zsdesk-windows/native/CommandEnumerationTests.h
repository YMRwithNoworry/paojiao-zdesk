#pragma once
#include "NativeCommandGuard.h"
#include <functional>

// Controlled public COM contracts only: no registry, real extension, popup or
// file command is used. The export is referenced by an exact Rust test.
namespace native_command_enumeration_probe {
using Microsoft::WRL::ComPtr;
using native_command_guard::Gate;
struct Counts{ULONG flags=0,enums=0,next=0,clones=0,before=0,invokes=0;};

class Enumerator final:public IEnumExplorerCommand{
    std::atomic<ULONG> refs_{1};
public:
    std::shared_ptr<Counts> counts=std::make_shared<Counts>();
    ComPtr<IExplorerCommand> item;
    ULONG total=0,cursor=0;
    HRESULT nextResult=S_OK,cloneResult=S_OK;
    bool nullClone=false;
    std::function<void()> onNext,onClone;
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid,void** output)override{
        if(!output)return E_POINTER;*output=nullptr;
        if(iid==IID_IUnknown||iid==IID_IEnumExplorerCommand||iid==IID_IAgileObject){*output=static_cast<IEnumExplorerCommand*>(this);AddRef();return S_OK;}
        return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef()override{return ++refs_;}
    ULONG STDMETHODCALLTYPE Release()override{const auto count=--refs_;if(!count)delete this;return count;}
    HRESULT STDMETHODCALLTYPE Next(ULONG count,IExplorerCommand** output,ULONG* fetched)override{
        ++counts->next;if(fetched)*fetched=0;if(!output||(!fetched&&count!=1))return E_INVALIDARG;
        for(ULONG i=0;i<count;++i)output[i]=nullptr;
        if(onNext)onNext();if(FAILED(nextResult))return nextResult;
        ULONG got=0;while(got<count&&cursor<total){if(!item)return E_UNEXPECTED;output[got]=item.Get();output[got++]->AddRef();++cursor;}
        if(fetched)*fetched=got;return got==count?S_OK:S_FALSE;
    }
    HRESULT STDMETHODCALLTYPE Skip(ULONG count)override{const auto remaining=total-cursor;cursor+=(std::min)(count,remaining);return count<=remaining?S_OK:S_FALSE;}
    HRESULT STDMETHODCALLTYPE Reset()override{cursor=0;return S_OK;}
    HRESULT STDMETHODCALLTYPE Clone(IEnumExplorerCommand** output)override{
        ++counts->clones;if(!output)return E_POINTER;*output=nullptr;if(onClone)onClone();
        if(FAILED(cloneResult)||nullClone)return cloneResult;
        ComPtr<Enumerator> copy;copy.Attach(new(std::nothrow) Enumerator);if(!copy)return E_OUTOFMEMORY;
        copy->counts=counts;copy->item=item;copy->total=total;copy->cursor=cursor;
        copy->nextResult=nextResult;copy->cloneResult=cloneResult;copy->nullClone=nullClone;copy->onNext=onNext;copy->onClone=onClone;
        *output=copy.Detach();return cloneResult;
    }
};

class Provider final:public IExplorerCommand{
    std::atomic<ULONG> refs_{1};
public:
    std::shared_ptr<Counts> counts=std::make_shared<Counts>();
    EXPCMDFLAGS flags=ECF_DEFAULT;
    HRESULT enumResult=E_NOTIMPL;
    ComPtr<IEnumExplorerCommand> children;
    std::function<void()> onEnum;
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid,void** output)override{
        if(!output)return E_POINTER;*output=nullptr;
        if(iid==IID_IUnknown||iid==IID_IExplorerCommand||iid==IID_IAgileObject){*output=static_cast<IExplorerCommand*>(this);AddRef();return S_OK;}
        return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef()override{return ++refs_;}
    ULONG STDMETHODCALLTYPE Release()override{const auto count=--refs_;if(!count)delete this;return count;}
    HRESULT STDMETHODCALLTYPE GetTitle(IShellItemArray*,LPWSTR* output)override{if(!output)return E_POINTER;*output=nullptr;return E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE GetIcon(IShellItemArray*,LPWSTR* output)override{if(!output)return E_POINTER;*output=nullptr;return E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE GetToolTip(IShellItemArray*,LPWSTR* output)override{if(!output)return E_POINTER;*output=nullptr;return E_NOTIMPL;}
    HRESULT STDMETHODCALLTYPE GetCanonicalName(GUID* output)override{if(!output)return E_POINTER;*output=GUID_NULL;return S_OK;}
    HRESULT STDMETHODCALLTYPE GetState(IShellItemArray*,BOOL,EXPCMDSTATE* output)override{if(!output)return E_POINTER;*output=ECS_ENABLED;return S_OK;}
    HRESULT STDMETHODCALLTYPE Invoke(IShellItemArray*,IBindCtx*)override{++counts->invokes;return S_OK;}
    HRESULT STDMETHODCALLTYPE GetFlags(EXPCMDFLAGS* output)override{++counts->flags;if(!output)return E_POINTER;*output=flags;return S_OK;}
    HRESULT STDMETHODCALLTYPE EnumSubCommands(IEnumExplorerCommand** output)override{
        ++counts->enums;if(!output)return E_POINTER;*output=nullptr;if(onEnum)onEnum();
        if(FAILED(enumResult))return enumResult;if(children){*output=children.Get();(*output)->AddRef();}return enumResult;
    }
};

HRESULT WINAPI Before(void* raw){++static_cast<Counts*>(raw)->before;return S_OK;}
struct Case{
    std::shared_ptr<Counts> counts=std::make_shared<Counts>();
    std::shared_ptr<Gate> gate;
    ComPtr<Provider> provider;
    ComPtr<IExplorerCommand> proxy;
    HRESULT Init(HWND owner,EXPCMDFLAGS flags,HRESULT result){
        provider.Attach(new(std::nothrow) Provider);if(!provider)return E_OUTOFMEMORY;
        provider->counts=counts;provider->flags=flags;provider->enumResult=result;
        HRESULT hr=Gate::Create(owner,&Before,counts.get(),nullptr,nullptr,0,nullptr,false,gate);if(FAILED(hr))return hr;
        gate->Generation(1);gate->Arm();return native_command_guard::WrapCommand(provider.Get(),gate,1,{},&proxy);
    }
    bool MetadataOnly()const{return counts->before==0&&counts->invokes==0&&gate->State().phase==native_command_guard::Phase::Fresh;}
};
struct Window{HWND value=nullptr;~Window(){if(value)DestroyWindow(value);}};

inline HRESULT Run(uint32_t& passed){
    Window owner{CreateWindowExW(0,L"STATIC",L"ZSDesk enumeration contract",WS_POPUP,0,0,1,1,nullptr,nullptr,GetModuleHandleW(nullptr),nullptr)};
    if(!owner.value)return HRESULT_FROM_WIN32(GetLastError());
    {
        Case test;HRESULT hr=test.Init(owner.value,ECF_HASSUBCOMMANDS,E_NOTIMPL);if(FAILED(hr))return hr;
        EXPCMDFLAGS flags=ECF_DEFAULT;hr=test.proxy->GetFlags(&flags);if(FAILED(hr))return hr;
        ComPtr<IEnumExplorerCommand> enumeration;hr=test.proxy->EnumSubCommands(&enumeration);
        if(hr!=E_NOTIMPL||enumeration||test.gate->State().failure!=E_NOTIMPL||test.counts->flags!=1||!test.MetadataOnly())return E_UNEXPECTED;
        passed|=1;
    }
    {
        Case test;HRESULT hr=test.Init(owner.value,ECF_HASSUBCOMMANDS,S_OK);if(FAILED(hr))return hr;
        ComPtr<IEnumExplorerCommand> enumeration;hr=test.proxy->EnumSubCommands(&enumeration);
        if(hr!=E_UNEXPECTED||enumeration||test.gate->State().failure!=E_UNEXPECTED||!test.MetadataOnly())return E_UNEXPECTED;
        passed|=2;
    }
    {
        Case test;HRESULT hr=test.Init(owner.value,ECF_DEFAULT,E_NOTIMPL);if(FAILED(hr))return hr;
        ComPtr<IEnumExplorerCommand> enumeration;hr=test.proxy->EnumSubCommands(&enumeration);
        if(hr!=E_NOTIMPL||enumeration||FAILED(test.gate->State().failure)||!test.MetadataOnly())return E_UNEXPECTED;
        passed|=4;
    }
    {
        Case test;HRESULT hr=test.Init(owner.value,ECF_HASSUBCOMMANDS,S_OK);if(FAILED(hr))return hr;
        ComPtr<Enumerator> original;original.Attach(new(std::nothrow) Enumerator);if(!original)return E_OUTOFMEMORY;test.provider->children=original;
        ComPtr<IEnumExplorerCommand> enumeration,clone;hr=test.proxy->EnumSubCommands(&enumeration);if(FAILED(hr))return hr;
        IExplorerCommand* items[2]{};ULONG fetched=99;hr=enumeration->Next(2,items,&fetched);
        if(hr!=S_FALSE||fetched||items[0]||items[1])return E_UNEXPECTED;
        hr=enumeration->Clone(&clone);if(FAILED(hr)||!clone)return E_UNEXPECTED;
        fetched=99;hr=clone->Next(1,items,&fetched);
        if(hr!=S_FALSE||fetched||items[0]||FAILED(test.gate->State().failure)||!test.MetadataOnly())return E_UNEXPECTED;
        passed|=8;
    }
    {
        Case test;HRESULT hr=test.Init(owner.value,ECF_HASSUBCOMMANDS,S_OK);if(FAILED(hr))return hr;
        ComPtr<Provider> leaf;leaf.Attach(new(std::nothrow) Provider);if(!leaf)return E_OUTOFMEMORY;
        ComPtr<Enumerator> original;original.Attach(new(std::nothrow) Enumerator);if(!original)return E_OUTOFMEMORY;
        original->item=leaf;original->total=1;test.provider->children=original;
        ComPtr<IEnumExplorerCommand> enumeration,clone;hr=test.proxy->EnumSubCommands(&enumeration);if(FAILED(hr))return hr;
        IExplorerCommand* items[2]{};ULONG fetched=0;hr=enumeration->Next(2,items,&fetched);
        ComPtr<IExplorerCommand> child;child.Attach(items[0]);
        if(hr!=S_FALSE||fetched!=1||!child||items[1])return E_UNEXPECTED;
        ComPtr<IEnumExplorerCommand> none;hr=child->EnumSubCommands(&none);if(hr!=E_NOTIMPL||none)return E_UNEXPECTED;
        hr=enumeration->Clone(&clone);if(FAILED(hr)||!clone)return E_UNEXPECTED;
        items[0]=nullptr;fetched=99;hr=clone->Next(1,items,&fetched);
        if(hr!=S_FALSE||fetched||items[0]||FAILED(test.gate->State().failure)||!test.MetadataOnly()||leaf->counts->invokes)return E_UNEXPECTED;
        passed|=16;
    }
    {
        for(unsigned operation=0;operation<3;++operation){
            Case test;HRESULT hr=test.Init(owner.value,ECF_HASSUBCOMMANDS,S_OK);if(FAILED(hr))return hr;
            ComPtr<Enumerator> original;original.Attach(new(std::nothrow) Enumerator);if(!original)return E_OUTOFMEMORY;
            original->nextResult=E_ACCESSDENIED;original->cloneResult=operation==2?S_OK:E_NOTIMPL;original->nullClone=operation==2;test.provider->children=original;
            ComPtr<IEnumExplorerCommand> enumeration,clone;hr=test.proxy->EnumSubCommands(&enumeration);if(FAILED(hr))return hr;
            IExplorerCommand* item=nullptr;ULONG fetched=99;
            hr=operation?enumeration->Clone(&clone):enumeration->Next(1,&item,&fetched);
            const HRESULT expected=operation==0?E_ACCESSDENIED:operation==1?E_NOTIMPL:E_UNEXPECTED;
            if(hr!=expected||item||clone||test.gate->State().failure!=expected||!test.MetadataOnly()||(!operation&&fetched))return E_UNEXPECTED;
        }
        passed|=32;
    }
    {
        Case test;HRESULT hr=test.Init(owner.value,ECF_HASSUBCOMMANDS,E_NOTIMPL);if(FAILED(hr))return hr;
        test.provider->onEnum=[gate=test.gate]{gate->Generation(2);gate->Arm();};
        ComPtr<IEnumExplorerCommand> enumeration;hr=test.proxy->EnumSubCommands(&enumeration);
        if(hr!=native_command_guard::kCancelled||enumeration||FAILED(test.gate->State().failure)||!test.MetadataOnly())return E_UNEXPECTED;
        passed|=64;
    }
    {
        for(bool cloning:{false,true}){
            Case test;HRESULT hr=test.Init(owner.value,ECF_HASSUBCOMMANDS,S_OK);if(FAILED(hr))return hr;
            ComPtr<Enumerator> original;original.Attach(new(std::nothrow) Enumerator);if(!original)return E_OUTOFMEMORY;
            const auto retire=[gate=test.gate]{gate->Generation(2);gate->Arm();};
            if(cloning)original->onClone=retire;else original->onNext=retire;
            original->nextResult=original->cloneResult=E_ACCESSDENIED;test.provider->children=original;
            ComPtr<IEnumExplorerCommand> enumeration,clone;hr=test.proxy->EnumSubCommands(&enumeration);if(FAILED(hr))return hr;
            IExplorerCommand* item=nullptr;ULONG fetched=99;
            hr=cloning?enumeration->Clone(&clone):enumeration->Next(1,&item,&fetched);
            if(hr!=native_command_guard::kCancelled||item||clone||FAILED(test.gate->State().failure)||!test.MetadataOnly())return E_UNEXPECTED;
            const auto before=original->counts->next;fetched=99;hr=enumeration->Next(1,&item,&fetched);
            if(hr!=native_command_guard::kCancelled||fetched||item||original->counts->next!=before||FAILED(test.gate->State().failure))return E_UNEXPECTED;
        }
        passed|=128;
    }
    return S_OK;
}
}

extern "C" HRESULT zs_menu_test_enumeration_contract(uint32_t* passed) noexcept{
    if(!passed)return E_POINTER;*passed=0;
    const HRESULT initialized=OleInitialize(nullptr);if(FAILED(initialized))return initialized;
    HRESULT result=E_UNEXPECTED;
    try{result=native_command_enumeration_probe::Run(*passed);}
    catch(const std::bad_alloc&){result=E_OUTOFMEMORY;}catch(...){result=E_UNEXPECTED;}
    OleUninitialize();return result;
}
