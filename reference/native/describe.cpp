// Isolated metadata probe. Values here belong to a temporary component, never a host baseline.
#include "provider.hpp"
#include <iostream>
using namespace atr;
int wmain(int argc,wchar_t** argv){
    if(argc!=2){std::cerr<<"Usage: describe.exe ABSOLUTE_PLUGIN_BINARY\n";return 2;}
    CoInitializeEx(nullptr,COINIT_APARTMENTTHREADED);
    HMODULE module=LoadLibraryExW(argv[1],nullptr,LOAD_WITH_ALTERED_SEARCH_PATH);
    if(!module){std::cout<<error("LoadLibrary failed: "+std::to_string(GetLastError()));return 1;}
    auto init=reinterpret_cast<bool(*)()>(GetProcAddress(module,"InitDll"));
    auto exitDll=reinterpret_cast<bool(*)()>(GetProcAddress(module,"ExitDll"));
    bool initialized=!init||init();int result=0;
    try{
        if(!initialized)throw std::runtime_error("InitDll failed");
        auto get=reinterpret_cast<IPluginFactory*(PLUGIN_API*)()>(GetProcAddress(module,"GetPluginFactory"));
        if(!get)throw std::runtime_error("GetPluginFactory missing");
        Ref<IPluginFactory> factory;factory.p=get();if(!factory.p)throw std::runtime_error("null factory");
        std::ostringstream out;out<<std::setprecision(17)<<"{\"ok\":true,\"source\":\"temporary_component_metadata_only\",\"classes\":[";bool first=true;
        const int count=factory->countClasses();if(count<0||count>1024)throw std::runtime_error("invalid class count");
        for(int i=0;i<count;++i){
            PClassInfo info{};if(factory->getClassInfo(i,&info)!=kResultOk)continue;
            if(std::strcmp(info.category,kVstAudioEffectClass))continue;
            if(!first)out<<',';first=false;
            out<<"{\"cid\":"<<json(hexid(info.cid))<<",\"name\":"<<json(info.name)<<",\"parameters\":[";
            ProbeProvider provider;provider.create(factory.p,info.cid,nullptr);auto* controller=provider.controller();
            if(!controller){out<<"]}";continue;}
            const int pc=controller->getParameterCount();if(pc<0||pc>10000)throw std::runtime_error("invalid parameter count");
            bool fp=true;
            for(int pi=0;pi<pc;++pi){ParameterInfo p{};if(controller->getParameterInfo(pi,p)!=kResultOk)continue;
                if(!fp)out<<',';fp=false;
                out<<"{\"id\":"<<p.id<<",\"title\":"<<json(text128(p.title))<<",\"step_count\":"<<p.stepCount
                   <<",\"flags\":"<<p.flags<<",\"temporary_default\":"<<controller->getParamNormalized(p.id)<<",\"options\":[";
                if(p.stepCount>0&&p.stepCount<=128)for(int n=0;n<=p.stepCount;++n){String128 label{};const double v=double(n)/p.stepCount;
                    if(controller->getParamStringByValue(p.id,v,label)!=kResultOk)throw std::runtime_error("option formatting failed");
                    if(n)out<<',';out<<"{\"label\":"<<json(text128(label))<<",\"normalized\":"<<v<<"}";
                }out<<"]}";
            }out<<"]}";
        }out<<"]}";std::cout<<out.str()<<std::endl;
    }catch(const std::exception& ex){std::cout<<error(ex.what())<<std::endl;result=1;}
    if(initialized&&exitDll)exitDll();FreeLibrary(module);CoUninitialize();return result;
}
