// Temporary method/metadata probe, separate from actual host instances.
#pragma once
#include "common.hpp"
#include "pluginterfaces/vst/ivstmessage.h"
namespace atr {
class ConnectionProxy final:public IConnectionPoint {
    IConnectionPoint* source;
    IConnectionPoint* destination=nullptr;
    std::atomic<uint32> references{1};
    const DWORD thread=GetCurrentThreadId();
public:
    explicit ConnectionProxy(IConnectionPoint* owner):source(owner){source->addRef();}
    ~ConnectionProxy(){if(destination)destination->release();source->release();}
    tresult PLUGIN_API queryInterface(const TUID id,void** out)override{
        if(!out)return kInvalidArgument;*out=nullptr;
        if(!memcmp(id,INLINE_UID_OF(IConnectionPoint),16)||!memcmp(id,INLINE_UID_OF(FUnknown),16)){*out=this;addRef();return kResultOk;}return kNoInterface;
    }
    uint32 PLUGIN_API addRef()override{return ++references;}
    uint32 PLUGIN_API release()override{auto result=--references;if(!result)delete this;return result;}
    tresult PLUGIN_API connect(IConnectionPoint* other)override{
        if(!other)return kInvalidArgument;if(destination)return kResultFalse;other->addRef();destination=other;
        auto result=source->connect(this);if(result!=kResultOk){destination=nullptr;other->release();}return result;
    }
    tresult PLUGIN_API disconnect(IConnectionPoint* other)override{
        if(other!=destination||!other)return kInvalidArgument;source->disconnect(this);destination=nullptr;other->release();return kResultOk;
    }
    tresult PLUGIN_API notify(IMessage* message)override{return destination&&GetCurrentThreadId()==thread?destination->notify(message):kResultFalse;}
};
class ProbeProvider {
    Ref<IComponent> componentRef;
    Ref<IEditController> controllerRef;
    Ref<IConnectionPoint> componentConnection,controllerConnection;
    ConnectionProxy* componentProxy=nullptr;
    ConnectionProxy* controllerProxy=nullptr;
    bool componentInitialized=false,controllerInitialized=false;
    bool componentConnected=false,controllerConnected=false;
    void resetOwned(){
        if(componentRef.p){auto* p=componentRef.p;componentRef.p=nullptr;p->release();}
        if(controllerRef.p){auto* p=controllerRef.p;controllerRef.p=nullptr;p->release();}
    }
public:
    IComponent* component()const{return componentRef.p;}
    IEditController* controller()const{return controllerRef.p;}
    ~ProbeProvider(){
        if(componentConnected)componentProxy->disconnect(controllerConnection.p);
        if(controllerConnected)controllerProxy->disconnect(componentConnection.p);
        if(componentProxy)componentProxy->release();if(controllerProxy)controllerProxy->release();
        bool combined=false;
        if(componentRef.p){Ref<IEditController> shared;combined=componentRef->queryInterface(INLINE_UID_OF(IEditController),shared.out())==kResultOk;}
        if(componentInitialized&&componentRef.p)componentRef->terminate();
        if(controllerInitialized&&controllerRef.p&&!combined)controllerRef->terminate();
        resetOwned();
    }
    void create(IPluginFactory* factory,const char* cid,FUnknown* context){
        if(factory->createInstance(cid,INLINE_UID_OF(IComponent),componentRef.out())!=kResultOk||!componentRef.p)
            throw std::runtime_error("profile CID unavailable");
        if(componentRef->initialize(context)!=kResultOk){resetOwned();throw std::runtime_error("temporary component initialize failed");}
        componentInitialized=true;
        if(componentRef->queryInterface(INLINE_UID_OF(IEditController),controllerRef.out())!=kResultOk||!controllerRef.p){
            TUID controllerId{};
            if(componentRef->getControllerClassId(controllerId)==kResultOk){
                factory->createInstance(controllerId,INLINE_UID_OF(IEditController),controllerRef.out());
                if(controllerRef.p){
                    if(controllerRef->initialize(context)!=kResultOk){componentInitialized=false;resetOwned();throw std::runtime_error("temporary controller initialize failed");}
                    controllerInitialized=true;
                }
            }
        }
        if(controllerRef.p){
            componentRef->queryInterface(INLINE_UID_OF(IConnectionPoint),componentConnection.out());
            controllerRef->queryInterface(INLINE_UID_OF(IConnectionPoint),controllerConnection.out());
            if(componentConnection.p&&controllerConnection.p){
                componentProxy=new ConnectionProxy(componentConnection.p);controllerProxy=new ConnectionProxy(controllerConnection.p);
                componentConnected=componentProxy->connect(controllerConnection.p)==kResultOk;
                if(componentConnected)controllerConnected=controllerProxy->connect(componentConnection.p)==kResultOk;
            }
        }
    }
};
}
