// Execute production identification/process code directly, without loading a
// VST3 file, starting the IPC worker, installing detours, or opening a host.
#include <algorithm>
#include "../native/agent.cpp"
#define FIXTURE_DLL
#include "fixture.cpp"

static int checks=0,failures=0;
#define CHECK(x) do{++checks;if(!(x)){++failures;std::cerr<<"FAIL line "<<__LINE__<<": " #x "\n";}}while(0)

static void signature(Group& group,unsigned index,uint32_t id,const wchar_t* title){
    group.valid[index].id=id;
    wcsncpy_s(group.valid[index].title.data(),128,title,_TRUNCATE);
}
static tresult PLUGIN_API originalProcess(IAudioProcessor* self,ProcessData& data){return self->process(data);}

static void snapshotChecks(){
    Plugin plugin(0);plugin.changeMetadataOnSecondPass=true;
    auto* processor=static_cast<IAudioProcessor*>(&plugin);auto* entry=slot(processor,9);
    groups[0].processEntry=entry;groups[0].validCount=2;
    // Sparse IDs, reversed relative to enumeration, full case-insensitive titles.
    signature(groups[0],0,61,L"humanize");signature(groups[0],1,4,L"RETUNE SPEED");
    groupCount.store(1);void* identity=nullptr;
    CHECK(identify(processor,entry,identity)==0);
    CHECK(identity==static_cast<IComponent*>(&plugin));
    CHECK(plugin.metadataPasses==1);
    CHECK(std::all_of(plugin.metadataReads.begin(),plugin.metadataReads.end(),[](auto n){return n==1;}));

    // A second candidate must use the same snapshot, including when the first
    // candidate failed. Re-querying creates an impossible mixed-time view.
    Plugin another(0);another.changeMetadataOnSecondPass=true;
    signature(groups[0],0,61,L"Not Humanize");groups[0].validCount=1;
    groups[1].processEntry=entry;groups[1].validCount=1;signature(groups[1],0,4,L"Retune Speed");groupCount.store(2);
    CHECK(identify(static_cast<IAudioProcessor*>(&another),entry,identity)==1);
    CHECK(another.metadataPasses==1);

    groupCount.store(1);signature(groups[0],0,4,L"Retune Speed ");
    Plugin exact(0);CHECK(identify(static_cast<IAudioProcessor*>(&exact),entry,identity)==-2);
    signature(groups[0],0,0,L"Retune Speed");
    CHECK(identify(static_cast<IAudioProcessor*>(&exact),entry,identity)==-2);
    signature(groups[0],0,4,L"Retune Speed");exact.failedMetadataOrdinal=0;
    CHECK(identify(static_cast<IAudioProcessor*>(&exact),entry,identity)==-2);
    Plugin split(2);groups[0].validCount=0;
    CHECK(identify(static_cast<IAudioProcessor*>(&split),entry,identity)==0);
    groups[0].validCount=1;
    CHECK(identify(static_cast<IAudioProcessor*>(&split),entry,identity)==-2);
}

static void cacheChecks(){
    Plugin plugin(0);auto* processor=static_cast<IAudioProcessor*>(&plugin);auto* entry=slot(processor,9);
    groups[0].processEntry=entry;groups[0].validCount=1;signature(groups[0],0,4,L"Retune Speed");
    groupCount.store(1);hooks[0]={entry,reinterpret_cast<void*>(&originalProcess),Kind::Process};
    auto* record=findInstance(processor);record->firstSeen.store(GetTickCount64()-4000);
    ProcessData data{};groups[0].cache.update(1,{{4,.25f}});
    CHECK(process(0,processor,data)==kResultOk&&plugin.stats.values[0]==.25);
    CHECK(record->group.load()==0);
    const auto metadataPasses=plugin.metadataPasses;

    // Simulate registration publication, not a user target/mapping change.
    // The cache must survive even though a broad second candidate also matches.
    groups[1].processEntry=entry;groups[1].validCount=0;groupCount.store(2);
    groups[0].cache.update(2,{{4,.75f}});
    CHECK(process(0,processor,data)==kResultOk&&plugin.stats.values[0]==.75);
    CHECK(record->group.load()==0&&plugin.metadataPasses==metadataPasses);
    CHECK(record->consumer.completion().revision==2&&record->consumer.completion().result==kResultOk);
    retire(processor);

    // A failed match is cached as well. A later profile does not retroactively
    // change that decision; a fresh instance can use the later profile.
    Plugin denied(1);auto* deniedProcessor=static_cast<IAudioProcessor*>(&denied);
    groupCount.store(1);record=findInstance(deniedProcessor);record->firstSeen.store(GetTickCount64()-4000);
    CHECK(process(0,deniedProcessor,data)==kResultOk&&record->group.load()==-2);
    const auto deniedPasses=denied.metadataPasses;
    groups[1].validCount=1;signature(groups[1],0,2004,L"Retune Speed");groupCount.store(2);
    groups[1].cache.update(1,{{2004,.5f}});
    CHECK(process(0,deniedProcessor,data)==kResultOk&&denied.stats.values[0]==0);
    CHECK(record->group.load()==-2&&denied.metadataPasses==deniedPasses);
    retire(deniedProcessor);
    record=findInstance(deniedProcessor);record->firstSeen.store(GetTickCount64()-4000);
    CHECK(process(0,deniedProcessor,data)==kResultOk&&denied.stats.values[0]==.5&&record->group.load()==1);
    retire(deniedProcessor);
}

static std::vector<char> profilePayload(const std::wstring& path,const TUID& cid,uint32_t flags,
                                      const std::vector<ValidParam>& rules){
    const auto bytes=static_cast<uint32_t>(path.size()*sizeof(wchar_t));
    const auto count=static_cast<uint32_t>(rules.size());
    std::vector<char> payload(28+bytes+260*count);
    memcpy(payload.data(),cid,16);memcpy(payload.data()+16,&flags,4);
    memcpy(payload.data()+20,&bytes,4);memcpy(payload.data()+24,&count,4);
    memcpy(payload.data()+28,path.data(),bytes);
    for(unsigned v=0;v<count;++v){
        memcpy(payload.data()+28+bytes+260*v,&rules[v].id,4);
        memcpy(payload.data()+32+bytes+260*v,rules[v].title.data(),256);
    }
    return payload;
}
static std::string preparePayload(const std::vector<char>& payload){
    Header request{};request.size=static_cast<uint32_t>(payload.size());
    try{return prepare(request,payload.data());}
    catch(const std::exception& error){return std::string("error: ")+error.what();}
}

static void duplicateRegistrationChecks(){
    // Seed a registered profile without loading a DLL or installing a hook.
    // The deliberately absent module lets negative identity tests take only the
    // GetModuleHandleEx pending branch, never GetPluginFactory/createInstance.
    const std::wstring path=L"Z:\\ATR-owned-test-9ab8cba2\\Fixture.vst3";
    TUID cid{};cid[0]=42;
    auto& original=groups[0];original.path=path;memcpy(original.cid,cid,16);
    original.validCount=1;signature(original,0,4,L"Retune Speed");
    original.freeComponent=true;original.contextNotNull=false;
    groupCount.store(1);
    original.cache.clear();original.cache.update(70,{{4,.375f}});
    original.received.store(8);original.duplicates.store(2);original.lastMessage.store(91);
    Snapshot before{};CHECK(original.cache.read(before));
    Plugin plugin(0);ProcessData data{};Consumer consumer;
    CHECK(deliver(original.cache,consumer,data,[&](auto& block){return plugin.process(block);})==kResultOk&&plugin.stats.values[0]==.375);
    const auto oldHooks=hookCount.load();
    auto payload=profilePayload(path,cid,1,{original.valid[0]});
    const auto expected="{\"ok\":true,\"group\":1,\"already_prepared\":true}";
    CHECK(preparePayload(payload)==expected);

    ValidParam changed{};changed.id=90;wcsncpy_s(changed.title.data(),128,L"Flex-Tune",_TRUNCATE);
    // Later configuration changes do not replace this candidate or its cache.
    CHECK(preparePayload(profilePayload(path,cid,1,{changed}))==expected);
    CHECK(preparePayload(profilePayload(path,cid,2,{}))==expected);
    CHECK(original.validCount==1&&original.valid[0].id==4&&wcscmp(original.valid[0].title.data(),L"Retune Speed")==0);
    CHECK(original.freeComponent&&!original.contextNotNull);
    Snapshot after{};CHECK(original.cache.read(after)&&after.serial==before.serial&&after.count==before.count);
    CHECK(original.received.load()==8&&original.duplicates.load()==2&&original.lastMessage.load()==91);
    CHECK(groupCount.load()==1&&hookCount.load()==oldHooks);
    // No second delivery or cache clear is introduced by duplicate registration.
    CHECK(deliver(original.cache,consumer,data,[&](auto& block){return plugin.process(block);})==kResultOk&&consumer.submissions.load()==1&&plugin.stats.values[0]==.375);
    Plugin fresh(0);void* identity=nullptr;
    CHECK(identify(static_cast<IAudioProcessor*>(&fresh),original.processEntry,identity)==0);

    TUID other{};other[0]=43;
    CHECK(preparePayload(profilePayload(path,other,1,{})).find("\"pending\":true")!=std::string::npos);
    CHECK(preparePayload(profilePayload(L"Z:\\ATR-owned-test-9ab8cba2\\Other.vst3",cid,1,{})).find("\"pending\":true")!=std::string::npos);
    // Reuse consumes no new capacity. Wire validation still precedes reuse.
    groupCount.store(MaxGroups);
    CHECK(preparePayload(profilePayload(path,cid,2,{}))==expected);
    groupCount.store(1);
    CHECK(preparePayload(profilePayload(path,cid,4,{})).find("invalid profile bounds")!=std::string::npos);
    changed.title.fill(L'x');
    CHECK(preparePayload(profilePayload(path,cid,1,{changed})).find("NUL terminated")!=std::string::npos);
}

int main(){
    // DllMain is linked as an ordinary function in this executable and is not called.
    snapshotChecks();cacheChecks();duplicateRegistrationChecks();
    std::cout<<checks<<" identification checks, "<<failures<<" failures\n";
    return failures?1:0;
}
