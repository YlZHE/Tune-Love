#pragma once
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <array>
#include <atomic>
#include <iomanip>
#include <sstream>
#include <stdexcept>
#include <string>
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"
#include "pluginterfaces/vst/ivsthostapplication.h"
namespace atr {
using namespace Steinberg;using namespace Steinberg::Vst;
inline std::string json(const std::string& value){
    std::ostringstream s;s<<'"';for(unsigned char c:value){if(c=='"'||c=='\\')s<<'\\'<<c;else if(c<32)s<<"\\u"<<std::hex<<std::setw(4)<<std::setfill('0')<<int(c)<<std::dec;else s<<c;}s<<'"';return s.str();
}
inline std::string utf8(const wchar_t* p,int n=-1){
    int size=WideCharToMultiByte(CP_UTF8,0,p,n,nullptr,0,nullptr,nullptr);if(size<=0)return {};
    std::string out(size,'\0');WideCharToMultiByte(CP_UTF8,0,p,n,out.data(),size,nullptr,nullptr);if(n<0)out.pop_back();return out;
}
inline std::string text128(const TChar* p){int n=0;while(n<128&&p[n])++n;return utf8(reinterpret_cast<const wchar_t*>(p),n);}
inline std::string hexid(const char* p){std::ostringstream s;for(int i=0;i<16;++i)s<<std::hex<<std::setw(2)<<std::setfill('0')<<unsigned(static_cast<unsigned char>(p[i]));return s.str();}
inline std::string error(const std::string& what){return "{\"ok\":false,\"error\":"+json(what)+"}";}
inline void* slot(void* object,unsigned index){return (*static_cast<void***>(object))[index];}
template<class T> struct Ref {
    T* p=nullptr;~Ref(){if(p)p->release();}T* operator->()const{return p;}
    Ref()=default;Ref(const Ref&)=delete;Ref& operator=(const Ref&)=delete;
    void** out(){return reinterpret_cast<void**>(&p);}
};
struct ValidParam { uint32_t id=0; std::array<wchar_t,128> title{}; };
class MetadataHost final:public IHostApplication {
public:
    tresult PLUGIN_API queryInterface(const TUID id,void** out)override{
        if(!out)return kInvalidArgument;*out=nullptr;
        if(!std::memcmp(id,INLINE_UID_OF(IHostApplication),16)||!std::memcmp(id,INLINE_UID_OF(FUnknown),16)){*out=this;return kResultOk;}return kNoInterface;
    }
    uint32 PLUGIN_API addRef()override{return 1;}
    uint32 PLUGIN_API release()override{return 1;}
    tresult PLUGIN_API getName(String128 name)override{const char16_t label[]=u"Reference metadata probe";std::memcpy(name,label,sizeof(label));return kResultOk;}
    tresult PLUGIN_API createInstance(TUID,TUID,void** out)override{if(out)*out=nullptr;return kNoInterface;}
};
// Profile strings come from the plugin's own parameter table.
inline bool sameTitle(const TChar* actual,const wchar_t* expected){
    unsigned a=0,b=0;for(;;){
        auto ca=a<128?static_cast<wchar_t>(actual[a]):0;auto cb=b<128?expected[b]:0;
        if(ca>=L'A'&&ca<=L'Z')ca+=L'a'-L'A';if(cb>=L'A'&&cb<=L'Z')cb+=L'a'-L'A';
        if(ca!=cb)return false;if(!ca)return true;++a;++b;
    }
}
}
