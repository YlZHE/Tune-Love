// Own IPC and method interception. No commercial runtime or binary offsets.
#include "common.hpp"
#include "delivery.hpp"
#include "provider.hpp"
#include "MinHook.h"
#include <sddl.h>
#include <tlhelp32.h>
#include <vector>
#include <mutex>
#include <unordered_map>
using namespace atr;
namespace {
constexpr unsigned MaxGroups=16,MaxInstances=128,MaxHooks=64,MaxValid=16;
#pragma pack(push,1)
struct Header {uint32_t magic;uint16_t version,op;uint32_t group,size;uint64_t serial;};
#pragma pack(pop)
static_assert(sizeof(Header)==24);
struct Group {
    HMODULE module=nullptr;std::wstring path;TUID cid{};void* processEntry=nullptr;
    unsigned validCount=0;std::array<ValidParam,MaxValid> valid{};
    bool freeComponent=true,contextNotNull=false;IComponent* retainedProbe=nullptr;
    Cache cache;std::atomic<uint64_t> received{0},duplicates{0},lastMessage{0};
};
std::array<Group,MaxGroups> groups;std::atomic<unsigned> groupCount{0};
std::atomic<bool> controlFailed{false};std::string fatalError;
struct Instance {
    std::atomic<void*> object{nullptr},component{nullptr};
    std::atomic<int> group{-1}; // -1 discovering, -2 unmatched, -3 ambiguous, -4 retired
    std::atomic<uint64_t> firstSeen{0},blocks{0};Consumer consumer;
    std::atomic_flag busy=ATOMIC_FLAG_INIT;
};
std::array<Instance,MaxInstances> instances;
std::atomic<uint64_t> overflow{0},reentries{0};
enum class Kind{Process,Release,Terminate};
struct Hook {void* entry=nullptr;void* original=nullptr;Kind kind=Kind::Process;};
std::array<Hook,MaxHooks> hooks;std::atomic<unsigned> hookCount{0};
HMODULE agentModule=nullptr;
using ProcessFn=tresult(PLUGIN_API*)(IAudioProcessor*,ProcessData&);
using ReleaseFn=uint32(PLUGIN_API*)(FUnknown*);
using TerminateFn=tresult(PLUGIN_API*)(IPluginBase*);

void retire(void* object){
    for(auto& i:instances)if(i.object.load()==object||i.component.load()==object){
        i.group.store(-4,std::memory_order_release);i.component.store(nullptr);i.object.store(nullptr,std::memory_order_release);
    }
}
void retireThroughInterface(FUnknown* candidate){
    if(!candidate)return;
    // Terminate receives the base subobject, including before the 4s discovery
    // threshold. Query while the object is alive, never after a final release.
    Ref<IAudioProcessor> processor;
    candidate->queryInterface(INLINE_UID_OF(IAudioProcessor),processor.out());
    if(processor.p)retire(processor.p);retire(candidate);
}
// Only borrowed pointer identities persist. No host instance references are retained.
int identify(IAudioProcessor* object,void* entry,void*& componentIdentity){
    Ref<IComponent> component;
    if(object->queryInterface(INLINE_UID_OF(IComponent),component.out())!=kResultOk||!component.p)return -1;
    componentIdentity=component.p;
    Ref<IEditController> controller;
    const auto controllerResult=component->queryInterface(INLINE_UID_OF(IEditController),controller.out());
    // Every candidate sees one parameter table from this identification attempt.
    // Re-enumerating for each signature can mix metadata from different states.
    std::unordered_map<ParamID,ParameterInfo> metadata;
    if(controllerResult==kResultOk&&controller.p){
        const int count=controller->getParameterCount();
        if(count>=0&&count<=10000){
            metadata.reserve(count);
            for(int k=0;k<count;++k){ParameterInfo info{};
                if(controller->getParameterInfo(k,info)==kResultOk)metadata[info.id]=info;
            }
        }
    }
    int selected=-2;
    for(unsigned g=0,n=groupCount.load(std::memory_order_acquire);g<n;++g){
        auto& profile=groups[g];if(profile.processEntry!=entry)continue;bool matches=true;
        for(unsigned pi=0;pi<profile.validCount&&matches;++pi){
            const auto found=metadata.find(profile.valid[pi].id);
            matches=found!=metadata.end()&&sameTitle(found->second.title,profile.valid[pi].title.data());
        }
        if(matches){if(selected>=0)return -3;selected=static_cast<int>(g);}
    }return selected;
}
Instance* findInstance(void* self){
    for(auto& i:instances)if(i.object.load(std::memory_order_acquire)==self)return &i;
    for(auto& i:instances){
        if(i.busy.test_and_set(std::memory_order_acquire))continue;
        if(i.object.load()==nullptr){
            i.component.store(nullptr);i.group.store(-1);i.firstSeen.store(GetTickCount64());i.blocks.store(0);
            i.consumer.last.store(0);i.consumer.submissions.store(0);i.consumer.processResult.store(kNotImplemented);
            i.consumer.publishCompletion(0,kNotImplemented);
            i.object.store(self,std::memory_order_release);i.busy.clear(std::memory_order_release);return &i;
        }i.busy.clear(std::memory_order_release);
    }overflow.fetch_add(1,std::memory_order_relaxed);return nullptr;
}
tresult process(unsigned index,IAudioProcessor* self,ProcessData& data){
    auto fn=reinterpret_cast<ProcessFn>(hooks[index].original);
    if(controlFailed.load(std::memory_order_acquire))return fn(self,data);
    auto* record=findInstance(self);if(!record)return fn(self,data);
    if(record->busy.test_and_set(std::memory_order_acquire)){reentries.fetch_add(1);return fn(self,data);}
    struct Unlock{Instance& i;~Unlock(){i.busy.clear(std::memory_order_release);}} unlock{*record};
    if(record->object.load()!=self)return fn(self,data);
    record->blocks.fetch_add(1,std::memory_order_relaxed);
    int selected=record->group.load(std::memory_order_acquire);
    // Mirrors the observed first-seen grace period. Identification may call plugin
    // QI/metadata once; steady-state delivery itself makes no allocations or IPC.
    // Registering a candidate does not invalidate an accepted or rejected this.
    if(selected==-1&&GetTickCount64()-record->firstSeen.load()>=4000){
        void* component=nullptr;const int previous=selected;selected=identify(self,hooks[index].entry,component);
        if(selected!=previous){record->consumer.last.store(0);record->consumer.publishCompletion(0,kNotImplemented);}
        record->component.store(component);record->group.store(selected,std::memory_order_release);
    }
    if(selected<0)return fn(self,data);
    return deliver(groups[selected].cache,record->consumer,data,[&](auto& d){return fn(self,d);});
}
template<unsigned N> tresult PLUGIN_API processThunk(IAudioProcessor* p,ProcessData& d){return process(N,p,d);}
template<unsigned N> uint32 PLUGIN_API releaseThunk(FUnknown* p){
    const auto result=reinterpret_cast<ReleaseFn>(hooks[N].original)(p);if(!result)retire(p);return result;
}
template<unsigned N> tresult PLUGIN_API terminateThunk(IPluginBase* p){
    retireThroughInterface(p);return reinterpret_cast<TerminateFn>(hooks[N].original)(p);
}
template<size_t... N> constexpr auto processTable(std::index_sequence<N...>){return std::array{&processThunk<N>...};}
template<size_t... N> constexpr auto releaseTable(std::index_sequence<N...>){return std::array{&releaseThunk<N>...};}
template<size_t... N> constexpr auto terminateTable(std::index_sequence<N...>){return std::array{&terminateThunk<N>...};}
constexpr auto processes=processTable(std::make_index_sequence<MaxHooks>{});
constexpr auto releases=releaseTable(std::make_index_sequence<MaxHooks>{});
constexpr auto terminates=terminateTable(std::make_index_sequence<MaxHooks>{});
void install(void* entry,Kind kind){
    const auto n=hookCount.load();for(unsigned i=0;i<n;++i)if(hooks[i].entry==entry){if(hooks[i].kind!=kind)throw std::runtime_error("incompatible shared hook signature");return;}
    if(n==MaxHooks)throw std::runtime_error("hook capacity reached");
#ifdef ATR_TEST_FAULTS
    wchar_t fault[16]{};
    if(GetEnvironmentVariableW(L"ATR_REFERENCE_FAIL_AFTER_HOOKS",fault,16)&&n==wcstoul(fault,nullptr,10))
        throw std::runtime_error("fixture requested hook failure");
#endif
    void* callback=kind==Kind::Process?reinterpret_cast<void*>(processes[n]):kind==Kind::Release?reinterpret_cast<void*>(releases[n]):reinterpret_cast<void*>(terminates[n]);
    void* original=nullptr;const auto created=MH_CreateHook(entry,callback,&original);
    if(created!=MH_OK)throw std::runtime_error("method hook creation failed: "+std::to_string(created));
    // Publish the trampoline before enabling: the detour can run immediately.
    hooks[n]={entry,original,kind};hookCount.store(n+1,std::memory_order_release);
    const auto enabled=MH_EnableHook(entry);
    if(enabled!=MH_OK)throw std::runtime_error("method hook enabling failed: "+std::to_string(enabled));
}
void conflictCheck(){
    HANDLE snap=CreateToolhelp32Snapshot(TH32CS_SNAPMODULE,GetCurrentProcessId());
    if(snap==INVALID_HANDLE_VALUE)throw std::runtime_error("module inspection failed");
    MODULEENTRY32W m{};m.dwSize=sizeof(m);bool conflict=false;
    if(!Module32FirstW(snap,&m)){CloseHandle(snap);throw std::runtime_error("module enumeration failed");}
    do{if(!_wcsicmp(m.szModule,L"em64.dll")||!_wcsicmp(m.szModule,L"em32.dll")||!_wcsicmp(m.szModule,L"autotune_agent.dll"))conflict=true;}while(Module32NextW(snap,&m));
    CloseHandle(snap);if(conflict)throw std::runtime_error("another parameter hook is loaded; use an isolated clean process");
}
std::string status(){
    std::ostringstream out;out<<"{\"ok\":"<<(fatalError.empty()?"true":"false")<<",\"error\":"<<json(fatalError)<<",\"pid\":"<<GetCurrentProcessId()<<",\"route\":\"direct_process_queue\",\"identification_delay_ms\":4000,\"instance_overflow\":"<<overflow.load()<<",\"reentries\":"<<reentries.load()<<",\"groups\":[";
    for(unsigned g=0,n=groupCount.load(std::memory_order_acquire);g<n;++g){
        if(g)out<<',';auto& p=groups[g];Snapshot s{};bool available=p.cache.read(s);
        out<<"{\"group\":"<<g+1<<",\"plugin\":"<<json(utf8(p.path.c_str()))<<",\"cid\":"<<json(hexid(p.cid))
           <<",\"message_serial\":"<<p.lastMessage.load()<<",\"received\":"<<p.received.load()<<",\"duplicates\":"<<p.duplicates.load()
           <<",\"snapshot_available\":"<<(available?"true":"false")<<",\"cache_revision\":"<<s.serial<<",\"cached_parameters\":"<<s.count<<",\"valid_params\":"<<p.validCount<<"}";
    }out<<"],\"instances\":[";bool first=true;
    for(unsigned index=0;index<instances.size();++index){auto& i=instances[index];auto* object=i.object.load(std::memory_order_acquire);if(!object)continue;
        const auto completion=i.consumer.completion();
        if(!first)out<<',';first=false;
        out<<"{\"index\":"<<index<<",\"object\":"<<reinterpret_cast<uintptr_t>(object)<<",\"group\":"<<(i.group.load()+1)
           <<",\"state\":"<<json(i.group.load()>=0?"matched":i.group.load()==-1?"discovering":i.group.load()==-3?"ambiguous":"unmatched")
           <<",\"blocks\":"<<i.blocks.load()<<",\"consumed_revision\":"<<i.consumer.last.load()<<",\"submitted\":"<<i.consumer.submissions.load()
           <<",\"last_process_result\":"<<i.consumer.processResult.load()
           <<",\"completed_revision\":"<<completion.revision<<",\"completion_result\":"<<completion.result<<"}";
    }out<<"]}";return out.str();
}
std::string prepare(const Header& h,const char* payload){
    if(h.group||h.serial||h.size<28)throw std::runtime_error("invalid prepare profile packet");
    uint32_t flags=0,pathSize=0,validCount=0;memcpy(&flags,payload+16,4);memcpy(&pathSize,payload+20,4);memcpy(&validCount,payload+24,4);
    if(flags>3||!pathSize||pathSize>2048||pathSize%2||validCount>MaxValid||h.size!=28+pathSize+validCount*260)
        throw std::runtime_error("invalid profile bounds");
    std::wstring path(pathSize/2,L'\0');memcpy(path.data(),payload+28,pathSize);
    if(path.size()<3||path[1]!=L':'||path.find(L'\0')!=std::wstring::npos)throw std::runtime_error("absolute local plugin binary required");
    // Validate the whole request even when its candidate is already registered.
    for(unsigned v=0;v<validCount;++v){wchar_t end=0;memcpy(&end,payload+32+pathSize+260*v+254,2);
        if(end)throw std::runtime_error("profile title must be NUL terminated");
    }
    const unsigned n=groupCount.load();
    for(unsigned i=0;i<n;++i)if(!_wcsicmp(groups[i].path.c_str(),path.c_str())&&!memcmp(groups[i].cid,payload,16)){
        // Candidate registration preserves the first entry, independently of
        // later config changes. Do not re-probe, replace rules or clear its queue.
        return "{\"ok\":true,\"group\":"+std::to_string(i+1)+",\"already_prepared\":true}";
    }
    if(n==MaxGroups)throw std::runtime_error("profile capacity reached");
    HMODULE module=nullptr;if(!GetModuleHandleExW(0,path.c_str(),&module))return "{\"ok\":true,\"pending\":true,\"reason\":\"plugin_not_loaded\"}";
    struct ModuleLease {HMODULE value;~ModuleLease(){if(value)FreeLibrary(value);}} moduleLease{module};
    // A module lease keeps trampoline code mapped; no host object is pinned.
    // Intentional process-lifetime lease: hot DLL removal is not exposed.
    auto get=reinterpret_cast<IPluginFactory*(PLUGIN_API*)()>(GetProcAddress(module,"GetPluginFactory"));
    if(!get)throw std::runtime_error("GetPluginFactory missing");
    Ref<IPluginFactory> factory;factory.p=get();if(!factory.p)throw std::runtime_error("null plugin factory");
    static MetadataHost context;ProbeProvider provider;provider.create(factory.p,payload,(flags&2)?&context:nullptr);
    IComponent* component=provider.component();Ref<IAudioProcessor> processor;
    if(component->queryInterface(INLINE_UID_OF(IAudioProcessor),processor.out())!=kResultOk||!processor.p)throw std::runtime_error("component has no processor");
    auto& g=groups[n];g.path=path;g.module=module;memcpy(g.cid,payload,16);g.processEntry=slot(processor.p,9);g.validCount=validCount;g.freeComponent=(flags&1)!=0;g.contextNotNull=(flags&2)!=0;
    for(unsigned v=0;v<validCount;++v){memcpy(&g.valid[v].id,payload+28+pathSize+260*v,4);memcpy(g.valid[v].title.data(),payload+32+pathSize+260*v,256);
    }
    moduleLease.value=nullptr; // Any partially installed trampoline also needs this lease.
    try {
        // Lifecycle observers are independent engineering guards around the observed process route.
        install(slot(component,4),Kind::Terminate);install(slot(component,2),Kind::Release);install(slot(processor.p,2),Kind::Release);
        install(g.processEntry,Kind::Process);groupCount.store(n+1,std::memory_order_release);
    } catch(const std::exception& e) {
        // Never free a trampoline that another thread could have entered.
        // A partial install is terminal for this agent; passthrough stays mapped.
        fatalError=std::string("partial hook setup; restart isolated host: ")+e.what();controlFailed.store(true,std::memory_order_release);throw;
    }
    if(!g.freeComponent){component->addRef();g.retainedProbe=component;}
    return "{\"ok\":true,\"group\":"+std::to_string(n+1)+",\"state\":\"awaiting_process_identification\",\"free_component\":"+(g.freeComponent?"true":"false")+"}";
}
std::string execute(const char* buffer,DWORD count){
    if(count<sizeof(Header))throw std::runtime_error("incomplete header");Header h{};memcpy(&h,buffer,sizeof(h));
    if(h.magic!=0x32525441||h.version!=1||h.size!=count-sizeof(h))throw std::runtime_error("invalid wire header");
    const auto* payload=buffer+sizeof(h);conflictCheck();
    if(h.op==1){if(h.size||h.serial)throw std::runtime_error("status accepts no payload");return status();}
    if(controlFailed.load())throw std::runtime_error(fatalError);
    if(h.op==5)return prepare(h,payload);
    if(!h.group||h.group>groupCount.load())throw std::runtime_error("unknown profile group");auto& g=groups[h.group-1];
    if(h.op==4){if(h.size||h.serial)throw std::runtime_error("clear accepts no payload");g.cache.clear();return "{\"ok\":true,\"cleared\":true,\"dsp_reset\":false}";}
    if(h.op!=3||!h.serial||!h.size||h.size%8||h.size>MaxParameters*8)throw std::runtime_error("invalid apply batch");
    std::array<Entry,MaxParameters> values{};memcpy(values.data(),payload,h.size);
    uint64_t revision=0;auto result=g.cache.update(h.serial,std::span(values.data(),h.size/8),&revision);g.received.fetch_add(1);g.lastMessage.store(h.serial);
    if(result==Update::Duplicate)g.duplicates.fetch_add(1);
    return "{\"ok\":true,\"stage\":\"cached\",\"duplicate\":"+std::string(result==Update::Duplicate?"true":"false")+",\"cache_revision\":"+std::to_string(revision)+"}";
}
bool complete(HANDLE pipe,OVERLAPPED& ov,BOOL ok,DWORD timeout,DWORD& count){
    if(ok)return true;if(GetLastError()!=ERROR_IO_PENDING)return false;
    if(WaitForSingleObject(ov.hEvent,timeout)!=WAIT_OBJECT_0){CancelIoEx(pipe,&ov);GetOverlappedResult(pipe,&ov,&count,TRUE);return false;}
    return GetOverlappedResult(pipe,&ov,&count,FALSE)!=FALSE;
}
DWORD WINAPI worker(void*){
    CoInitializeEx(nullptr,COINIT_MULTITHREADED);
    // Keep the agent mapped for the lifetime of installed entry hooks.
    HMODULE pinned=nullptr;GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS|GET_MODULE_HANDLE_EX_FLAG_PIN,reinterpret_cast<LPCWSTR>(&worker),&pinned);
    if(MH_Initialize()!=MH_OK)return 1;
    HANDLE token=nullptr;if(!OpenProcessToken(GetCurrentProcess(),TOKEN_QUERY,&token))return 2;
    DWORD size=0;GetTokenInformation(token,TokenUser,nullptr,0,&size);std::vector<char> user(size);
    if(!GetTokenInformation(token,TokenUser,user.data(),size,&size)){CloseHandle(token);return 3;}CloseHandle(token);
    LPWSTR sid=nullptr;if(!ConvertSidToStringSidW(reinterpret_cast<TOKEN_USER*>(user.data())->User.Sid,&sid))return 4;
    std::wstring sddl=L"D:P(A;;GA;;;"+std::wstring(sid)+L")";LocalFree(sid);
    PSECURITY_DESCRIPTOR descriptor=nullptr;if(!ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.c_str(),SDDL_REVISION_1,&descriptor,nullptr))return 5;
    SECURITY_ATTRIBUTES sa{sizeof(sa),descriptor,FALSE};std::wstring name=L"\\\\.\\pipe\\autotune-reference-"+std::to_wstring(GetCurrentProcessId());
    HANDLE pipe=CreateNamedPipeW(name.c_str(),PIPE_ACCESS_DUPLEX|FILE_FLAG_OVERLAPPED|FILE_FLAG_FIRST_PIPE_INSTANCE,PIPE_TYPE_MESSAGE|PIPE_READMODE_MESSAGE|PIPE_WAIT|PIPE_REJECT_REMOTE_CLIENTS,1,65536,8192,5000,&sa);
    LocalFree(descriptor);if(pipe==INVALID_HANDLE_VALUE)return 6;HANDLE event=CreateEventW(nullptr,TRUE,FALSE,nullptr);if(!event){CloseHandle(pipe);return 7;}
    for(;;){
        OVERLAPPED ov{};ov.hEvent=event;ResetEvent(event);DWORD count=0;
        BOOL ok=ConnectNamedPipe(pipe,&ov);if(!ok&&GetLastError()==ERROR_PIPE_CONNECTED)ok=TRUE;
        if(!complete(pipe,ov,ok,INFINITE,count)){DisconnectNamedPipe(pipe);continue;}
        std::array<char,8192> buffer{};ov={};ov.hEvent=event;ResetEvent(event);
        ok=ReadFile(pipe,buffer.data(),static_cast<DWORD>(buffer.size()),&count,&ov);std::string response;
        try{if(!complete(pipe,ov,ok,5000,count))throw std::runtime_error("incomplete request");response=execute(buffer.data(),count);}catch(const std::exception& e){response=error(e.what());}
        if(response.size()>65000)response=error("status exceeds protocol capacity");
        ov={};ov.hEvent=event;ResetEvent(event);count=0;ok=WriteFile(pipe,response.data(),static_cast<DWORD>(response.size()),&count,&ov);complete(pipe,ov,ok,5000,count);DisconnectNamedPipe(pipe);
    }
}
}
BOOL WINAPI DllMain(HINSTANCE module,DWORD reason,LPVOID){
    if(reason==DLL_PROCESS_ATTACH){agentModule=module;DisableThreadLibraryCalls(module);HANDLE thread=CreateThread(nullptr,0,worker,nullptr,0,nullptr);if(thread)CloseHandle(thread);}return TRUE;
}
