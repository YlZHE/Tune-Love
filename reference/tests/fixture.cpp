// Owned VST3 fixture and a windowless host. Used only by regression tests.
#include "../native/common.hpp"
#include "../native/delivery.hpp"
#include <iostream>
#include <vector>
#include <mutex>
#include <thread>
using namespace atr;
struct Stats {uint64_t blocks=0,control=0,host=0,bad=0,failures=0;double values[6]{},energy=0;};
constexpr uint32_t BaseIds[]{4,90,62,61,17,162};
constexpr const wchar_t* Titles[]{L"Retune Speed",L"Flex-Tune",L"Natural Vibrato",L"Humanize",L"Key",L"Modern Scale"};
#ifdef ALT_PROFILE
constexpr unsigned Offset=1000;
#else
constexpr unsigned Offset=0;
#endif
#ifdef FIXTURE_DLL
void makeCid(int index,TUID cid){for(int n=0;n<16;++n)cid[n]=static_cast<char>(n*7+1);cid[0]=static_cast<char>(Offset?20:10);cid[1]=static_cast<char>(index);}
class Plugin final:public IComponent,public IAudioProcessor,public IEditController {
    std::atomic<uint32> refs{1};
public:
    Stats stats;bool fail=false;const unsigned shift;const bool noController;
    unsigned metadataPasses=0;std::array<unsigned,6> metadataReads{};
    bool changeMetadataOnSecondPass=false;int failedMetadataOrdinal=-1;
    explicit Plugin(int kind):shift(Offset+static_cast<unsigned>(kind)*2000),noController(kind==2){}
    tresult PLUGIN_API queryInterface(const TUID id,void** out)override{
        if(!out)return kInvalidArgument;*out=nullptr;
        if(!memcmp(id,INLINE_UID_OF(IComponent),16)||!memcmp(id,INLINE_UID_OF(FUnknown),16)||!memcmp(id,INLINE_UID_OF(IPluginBase),16))*out=static_cast<IComponent*>(this);
        else if(!memcmp(id,INLINE_UID_OF(IAudioProcessor),16))*out=static_cast<IAudioProcessor*>(this);
        else if(!noController&&!memcmp(id,INLINE_UID_OF(IEditController),16))*out=static_cast<IEditController*>(this);
        else return kNoInterface;addRef();return kResultOk;
    }
    uint32 PLUGIN_API addRef()override{return ++refs;}
    __declspec(noinline) uint32 PLUGIN_API release()override{auto n=--refs;if(!n)delete this;return n;}
    tresult PLUGIN_API initialize(FUnknown*)override{return kResultOk;}
    __declspec(noinline) tresult PLUGIN_API terminate()override{stats.energy=0;return kResultOk;}
    tresult PLUGIN_API getControllerClassId(TUID)override{return kResultFalse;}
    tresult PLUGIN_API setIoMode(IoMode)override{return kResultOk;}
    int32 PLUGIN_API getBusCount(MediaType type,BusDirection)override{return type==kAudio?1:0;}
    tresult PLUGIN_API getBusInfo(MediaType type,BusDirection dir,int32 i,BusInfo& out)override{if(type!=kAudio||i)return kInvalidArgument;out={};out.mediaType=type;out.direction=dir;out.channelCount=1;out.busType=kMain;return kResultOk;}
    tresult PLUGIN_API getRoutingInfo(RoutingInfo&,RoutingInfo&)override{return kNotImplemented;}
    tresult PLUGIN_API activateBus(MediaType,BusDirection,int32,TBool)override{return kResultOk;}
    tresult PLUGIN_API setActive(TBool)override{return kResultOk;}
    tresult PLUGIN_API setState(IBStream*)override{return kResultOk;}
    tresult PLUGIN_API getState(IBStream*)override{return kResultOk;}
    tresult PLUGIN_API setBusArrangements(SpeakerArrangement*,int32,SpeakerArrangement*,int32)override{return kResultOk;}
    tresult PLUGIN_API getBusArrangement(BusDirection,int32,SpeakerArrangement& out)override{out=SpeakerArr::kMono;return kResultOk;}
    tresult PLUGIN_API canProcessSampleSize(int32 n)override{return n==kSample32?kResultOk:kResultFalse;}
    uint32 PLUGIN_API getLatencySamples()override{return 0;}
    tresult PLUGIN_API setupProcessing(ProcessSetup&)override{return kResultOk;}
    tresult PLUGIN_API setProcessing(TBool)override{return kResultOk;}
    __declspec(noinline) tresult PLUGIN_API process(ProcessData& data)override{
        ++stats.blocks;bool controlled=false,sentinel=false;
        if(data.inputParameterChanges)for(int q=0;q<data.inputParameterChanges->getParameterCount();++q){
            auto* queue=data.inputParameterChanges->getParameterData(q);if(!queue)continue;
            for(int point=0;point<queue->getPointCount();++point){int32 offset=0;double value=0;if(queue->getPoint(point,offset,value)!=kResultOk)continue;
                if(offset!=0)++stats.bad;
                const auto id=queue->getParameterId();if(id==99999)sentinel=true;
                for(int i=0;i<6;++i)if(id==BaseIds[i]+shift){controlled=true;if(!fail)stats.values[i]=value;}
            }
        }
        if(controlled)++stats.control;if(sentinel)++stats.host;if(controlled&&sentinel)++stats.bad;
        if(fail){++stats.failures;return kResultFalse;}
        double gain=1;for(auto v:stats.values)gain+=v;stats.energy=0;
        if(data.numOutputs&&data.numInputs)for(int n=0;n<data.numSamples;++n){const float value=static_cast<float>(data.inputs[0].channelBuffers32[0][n]*gain);data.outputs[0].channelBuffers32[0][n]=value;stats.energy+=double(value)*value;}
        return kResultOk;
    }
    uint32 PLUGIN_API getTailSamples()override{return 0;}
    tresult PLUGIN_API setComponentState(IBStream*)override{return kResultOk;}
    int32 PLUGIN_API getParameterCount()override{++metadataPasses;return 6;}
    tresult PLUGIN_API getParameterInfo(int32 i,ParameterInfo& info)override{
        if(i<0||i>=6)return kInvalidArgument;++metadataReads[i];info={};info.id=BaseIds[i]+shift;
        const wchar_t* title=changeMetadataOnSecondPass&&metadataPasses>1?L"Changed metadata":Titles[i];
        wcsncpy_s(reinterpret_cast<wchar_t*>(info.title),128,title,_TRUNCATE);
        info.flags=ParameterInfo::kCanAutomate;info.stepCount=i==4?11:i==5?14:0;
        return i==failedMetadataOrdinal?kResultFalse:kResultOk;
    }
    tresult PLUGIN_API getParamStringByValue(ParamID,double value,String128 out)override{swprintf_s(reinterpret_cast<wchar_t*>(out),128,L"%.5f",value);return kResultOk;}
    tresult PLUGIN_API getParamValueByString(ParamID,TChar*,double&)override{return kNotImplemented;}
    double PLUGIN_API normalizedParamToPlain(ParamID,double v)override{return v;}
    double PLUGIN_API plainParamToNormalized(ParamID,double v)override{return v;}
    double PLUGIN_API getParamNormalized(ParamID id)override{for(int i=0;i<6;++i)if(id==BaseIds[i]+shift)return stats.values[i];return 0;}
    tresult PLUGIN_API setParamNormalized(ParamID,double)override{return kNotImplemented;}
    tresult PLUGIN_API setComponentHandler(IComponentHandler*)override{return kResultOk;}
    IPlugView* PLUGIN_API createView(FIDString)override{return nullptr;}
};
class Factory final:public IPluginFactory {
public:
    tresult PLUGIN_API queryInterface(const TUID id,void** out)override{if(!out)return kInvalidArgument;*out=nullptr;if(!memcmp(id,INLINE_UID_OF(IPluginFactory),16)||!memcmp(id,INLINE_UID_OF(FUnknown),16)){*out=this;return kResultOk;}return kNoInterface;}
    uint32 PLUGIN_API addRef()override{return 1;}uint32 PLUGIN_API release()override{return 1;}
    tresult PLUGIN_API getFactoryInfo(PFactoryInfo* out)override{if(!out)return kInvalidArgument;*out=PFactoryInfo("Independent fixture","","",0);return kResultOk;}
    int32 PLUGIN_API countClasses()override{return 3;}
    tresult PLUGIN_API getClassInfo(int32 i,PClassInfo* out)override{if(!out||i<0||i>2)return kInvalidArgument;TUID cid{};makeCid(i,cid);*out=PClassInfo(cid,PClassInfo::kManyInstances,kVstAudioEffectClass,i==2?"No controller fixture":i?"Reference sibling":"Reference fixture");return kResultOk;}
    tresult PLUGIN_API createInstance(FIDString cid,FIDString id,void** out)override{if(!out)return kInvalidArgument;*out=nullptr;for(int i=0;i<3;++i){TUID own{};makeCid(i,own);if(!memcmp(cid,own,16)){auto* object=new Plugin(i);auto r=object->queryInterface(id,out);object->release();return r;}}return kInvalidArgument;}
};
extern "C" __declspec(dllexport) IPluginFactory* PLUGIN_API GetPluginFactory(){static Factory f;return &f;}
extern "C" __declspec(dllexport) bool InitDll(){return true;}
extern "C" __declspec(dllexport) bool ExitDll(){return true;}
extern "C" __declspec(dllexport) void FixtureInspect(IAudioProcessor* p,Stats* out){*out=static_cast<Plugin*>(p)->stats;}
extern "C" __declspec(dllexport) void FixtureFail(IAudioProcessor* p,bool value){static_cast<Plugin*>(p)->fail=value;}
extern "C" __declspec(dllexport) void FixtureParamInfo(IComponent* p,int index,ParameterInfo* out){static_cast<Plugin*>(p)->getParameterInfo(index,*out);}
#else
using Inspect=void(*)(IAudioProcessor*,Stats*);using Fail=void(*)(IAudioProcessor*,bool);
using Info=void(*)(IComponent*,int,ParameterInfo*);
struct Module {HMODULE dll;IPluginFactory* factory;std::wstring path;std::vector<PClassInfo> classes;Inspect inspect;Fail fail;Info info;};
struct Object {unsigned module,kind;IComponent* component;IAudioProcessor* processor;uint64_t restorationErrors=0;};
std::vector<Module> modules;std::vector<Object> objects;std::mutex hostMutex;std::atomic<bool> running{true};
MetadataHost host;
unsigned add(unsigned m,unsigned kind){
    if(m>=modules.size()||kind>=modules[m].classes.size())throw std::runtime_error("invalid module or class");
    auto& module=modules[m];Object object{m,kind,nullptr,nullptr};
    if(module.factory->createInstance(module.classes[kind].cid,INLINE_UID_OF(IComponent),reinterpret_cast<void**>(&object.component))!=kResultOk||!object.component)throw std::runtime_error("create failed");
    object.component->initialize(&host);object.component->queryInterface(INLINE_UID_OF(IAudioProcessor),reinterpret_cast<void**>(&object.processor));
    ProcessSetup setup{kRealtime,kSample32,64,48000};object.processor->setupProcessing(setup);object.component->setActive(true);object.processor->setProcessing(true);
    objects.push_back(object);return static_cast<unsigned>(objects.size()-1);
}
void drop(unsigned i){if(i>=objects.size()||!objects[i].component)throw std::runtime_error("invalid instance");auto& object=objects[i];object.processor->setProcessing(false);object.component->setActive(false);object.component->terminate();object.processor->release();object.component->release();object.processor=nullptr;object.component=nullptr;}
unsigned load(const std::wstring& path){
    HMODULE dll=LoadLibraryExW(path.c_str(),nullptr,LOAD_WITH_ALTERED_SEARCH_PATH);if(!dll)throw std::runtime_error("fixture load failed");
    auto init=reinterpret_cast<bool(*)()>(GetProcAddress(dll,"InitDll"));if(init&&!init())throw std::runtime_error("fixture InitDll failed");
    auto get=reinterpret_cast<IPluginFactory*(PLUGIN_API*)()>(GetProcAddress(dll,"GetPluginFactory"));if(!get)throw std::runtime_error("no factory");
    Module m{dll,get(),path,{},reinterpret_cast<Inspect>(GetProcAddress(dll,"FixtureInspect")),reinterpret_cast<Fail>(GetProcAddress(dll,"FixtureFail")),reinterpret_cast<Info>(GetProcAddress(dll,"FixtureParamInfo"))};
    for(int i=0;i<m.factory->countClasses();++i){PClassInfo c{};m.factory->getClassInfo(i,&c);m.classes.push_back(c);}modules.push_back(m);return static_cast<unsigned>(modules.size()-1);
}
std::string moduleJson(){
    std::ostringstream out;out<<"{\"ok\":true,\"pid\":"<<GetCurrentProcessId()<<",\"modules\":[";
    for(unsigned i=0;i<modules.size();++i){auto& m=modules[i];if(i)out<<',';out<<"{\"index\":"<<i<<",\"path\":"<<json(utf8(m.path.c_str()))<<",\"classes\":[";
        for(unsigned c=0;c<m.classes.size();++c){if(c)out<<',';out<<"{\"cid\":"<<json(hexid(m.classes[c].cid))<<",\"ids\":[";
            Ref<IComponent> p;m.factory->createInstance(m.classes[c].cid,INLINE_UID_OF(IComponent),p.out());
            for(int k=0;k<6;++k){ParameterInfo info{};m.info(p.p,k,&info);if(k)out<<',';out<<info.id;}
            out<<"],\"titles\":[";for(int k=0;k<6;++k){if(k)out<<',';out<<json(utf8(Titles[k]));}out<<"]}";
        }out<<"]}";
    }out<<"]}";return out.str();
}
std::string status(){std::ostringstream out;out<<std::setprecision(17)<<"{\"ok\":true,\"instances\":[";bool first=true;
    for(unsigned i=0;i<objects.size();++i){auto& object=objects[i];if(!object.component)continue;if(!first)out<<',';first=false;Stats s;modules[object.module].inspect(object.processor,&s);
        out<<"{\"index\":"<<i<<",\"module\":"<<object.module<<",\"class\":"<<object.kind<<",\"blocks\":"<<s.blocks<<",\"control_blocks\":"<<s.control<<",\"host_blocks\":"<<s.host<<",\"bad_offsets\":"<<s.bad<<",\"failures\":"<<s.failures<<",\"restoration_errors\":"<<object.restorationErrors<<",\"energy\":"<<s.energy<<",\"values\":[";
        for(int p=0;p<6;++p){if(p)out<<',';out<<s.values[p];}out<<"]}";
    }out<<"]}";return out.str();
}
void audio(){
    Snapshot s{};s.count=1;s.entries[0]={99999,.375f};Changes hostQueue(s);
    while(running){
        {std::lock_guard lock(hostMutex);for(auto& o:objects)if(o.processor){
            float in[64],out[64]{};for(int n=0;n<64;++n)in[n]=static_cast<float>(.1*std::sin(n*.2));float* inputs[]{in};float* outputs[]{out};
            AudioBusBuffers ib{},ob{};ib.numChannels=ob.numChannels=1;ib.channelBuffers32=inputs;ob.channelBuffers32=outputs;
            ProcessData data{};data.processMode=kRealtime;data.symbolicSampleSize=kSample32;data.numSamples=64;data.numInputs=data.numOutputs=1;data.inputs=&ib;data.outputs=&ob;data.inputParameterChanges=&hostQueue;
            o.processor->process(data);if(data.inputParameterChanges!=&hostQueue)++o.restorationErrors;
        }}Sleep(5);
    }
}
int wmain(int argc,wchar_t** argv){
    try{
        if(argc>1){load(argv[1]);add(0,0);add(0,0);}std::cout<<moduleJson()<<std::endl;std::thread thread(audio);
        std::string line;
        while(std::getline(std::cin,line)){
            bool quit=false;std::string response;
            try{std::lock_guard lock(hostMutex);std::istringstream in(line);std::string op;in>>op;
                if(op=="status")response=status();
                else if(op=="add"){unsigned m=0,c=0;in>>m>>c;response="{\"ok\":true,\"index\":"+std::to_string(add(m,c))+"}";}
                else if(op=="drop"){unsigned i;in>>i;drop(i);response="{\"ok\":true}";}
                else if(op=="fail"){unsigned i;int value;in>>i>>value;if(i>=objects.size()||!objects[i].processor)throw std::runtime_error("invalid instance");modules[objects[i].module].fail(objects[i].processor,value!=0);response="{\"ok\":true}";}
                else if(op=="load"){std::string path;std::getline(in>>std::ws,path);int count=MultiByteToWideChar(CP_UTF8,MB_ERR_INVALID_CHARS,path.data(),static_cast<int>(path.size()),nullptr,0);std::wstring wide(count,L'\0');MultiByteToWideChar(CP_UTF8,0,path.data(),static_cast<int>(path.size()),wide.data(),count);auto m=load(wide);add(m,0);response=moduleJson();}
                else if(op=="quit"){quit=true;response="{\"ok\":true}";}
                else throw std::runtime_error("unknown command");
            }catch(const std::exception& e){response=error(e.what());}
            std::cout<<response<<std::endl;if(quit)break;
        }
        running=false;thread.join();for(unsigned i=0;i<objects.size();++i)if(objects[i].component)drop(i);
        for(auto& m:modules){m.factory->release();auto exitDll=reinterpret_cast<bool(*)()>(GetProcAddress(m.dll,"ExitDll"));if(exitDll)exitDll();FreeLibrary(m.dll);}return 0;
    }catch(const std::exception& e){std::cout<<error(e.what())<<std::endl;return 1;}
}
#endif
