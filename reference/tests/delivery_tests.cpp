#include <iostream>
#if !__has_include("../native/delivery.hpp")
int main(){std::cerr<<"FAIL: direct process delivery engine is not implemented\n";return 1;}
#else
#include "../native/delivery.hpp"
#include <thread>
#include <limits>
using namespace atr;
static int checks=0;
auto completionOf(const Consumer& consumer){return consumer.completion();}
#define CHECK(x) do{++checks;if(!(x)){std::cerr<<"FAIL line "<<__LINE__<<": " #x "\n";return 1;}}while(0)
int main(){
    // Losing cached IDs, value replacement, or message dedup breaks these consumers.
    Cache cache;Snapshot snapshot{};
    CHECK(cache.update(1,{{4,.25f},{90,.5f}})==Update::Applied);
    CHECK(cache.read(snapshot)&&snapshot.count==2&&snapshot.serial==1);
    CHECK(cache.update(1,{{4,.75f}})==Update::Duplicate);
    CHECK(cache.read(snapshot)&&snapshot.entries[0].value==.25f);
    CHECK(cache.update(2,{{4,.75f}})==Update::Applied);
    CHECK(cache.read(snapshot)&&snapshot.count==2&&snapshot.entries[0].value==.75f&&snapshot.entries[1].id==90);
    bool invalid=false;try{cache.update(3,{{4,.5f},{90,std::numeric_limits<float>::infinity()}});}catch(const std::invalid_argument&){invalid=true;}
    CHECK(invalid&&cache.read(snapshot)&&snapshot.serial==2&&snapshot.entries[0].value==.75f);
    cache.clear();CHECK(cache.read(snapshot)&&snapshot.count==0&&snapshot.serial==2);
    CHECK(cache.update(2,{{4,.5f}})==Update::Duplicate);
    CHECK(cache.update(3,{{61,.8f}})==Update::Applied);
    CHECK(cache.read(snapshot)&&snapshot.count==1&&snapshot.entries[0].id==61);

    // Exercise the actual wrapper: direct queue, pointer restoration and per-instance gating.
    Snapshot hostSnapshot{};hostSnapshot.count=1;hostSnapshot.entries[0]={99999,.375f};
    Changes hostQueue(hostSnapshot);Steinberg::Vst::ProcessData data{};data.inputParameterChanges=&hostQueue;
    Consumer a,b;int calls=0;
    auto original=[&](auto& d){++calls;auto* q=d.inputParameterChanges->getParameterData(0);
        Steinberg::int32 offset=-1;double value=-1;q->getPoint(0,offset,value);
        if(q->getParameterId()!=61||offset!=0||std::abs(value-.8)>1e-6)return Steinberg::kInvalidArgument;
        return Steinberg::kResultOk;};
    CHECK(deliver(cache,a,data,original)==Steinberg::kResultOk&&calls==1);
    CHECK(data.inputParameterChanges==&hostQueue&&a.last.load()==3&&a.submissions.load()==1);
    bool passedHost=false;
    auto hostOriginal=[&](auto& d){passedHost=d.inputParameterChanges==&hostQueue;return Steinberg::kResultOk;};
    CHECK(deliver(cache,a,data,hostOriginal)==Steinberg::kResultOk&&passedHost&&a.submissions.load()==1);
    CHECK(deliver(cache,b,data,original)==Steinberg::kResultOk&&b.submissions.load()==1);
    cache.update(4,{{61,.8f}});
    CHECK(deliver(cache,a,data,original)==Steinberg::kResultOk&&a.submissions.load()==2);
    cache.update(5,{{61,.8f}});
    auto failing=[](auto&){return Steinberg::kResultFalse;};
    CHECK(deliver(cache,a,data,failing)==Steinberg::kResultFalse&&a.last.load()==5);
    passedHost=false;deliver(cache,a,data,hostOriginal);CHECK(passedHost&&a.submissions.load()==3);
    cache.clear();Consumer c;passedHost=false;deliver(cache,c,data,hostOriginal);
    CHECK(passedHost&&c.last.load()==5&&c.submissions.load()==0);
    CHECK(data.inputParameterChanges==&hostQueue);
    cache.update(6,{{61,.1f}});
    bool threw=false;try{deliver(cache,a,data,[](auto&)->Steinberg::tresult{throw 1;});}catch(int){threw=true;}
    CHECK(threw&&data.inputParameterChanges==&hostQueue);

    // Consumption is published before the host call, but completion must not
    // be visible until that exact call returns.  This catches reusing the
    // previous process result for a newer cache revision.
    Cache inFlightCache;inFlightCache.update(1,{{61,.25f}});
    Consumer inFlight;std::atomic<bool> entered=false,release=false;Steinberg::tresult inFlightResult=Steinberg::kNotImplemented;
    std::thread blocked([&]{inFlightResult=deliver(inFlightCache,inFlight,data,[&](auto&){entered.store(true,std::memory_order_release);while(!release.load(std::memory_order_acquire))std::this_thread::yield();return Steinberg::kResultOk;});});
    while(!entered.load(std::memory_order_acquire))std::this_thread::yield();
    auto during=completionOf(inFlight);
    auto attempts=inFlight.submissions.load();
    release.store(true,std::memory_order_release);blocked.join();
    CHECK(attempts==1&&during.revision==0&&during.result==Steinberg::kNotImplemented);
    CHECK(inFlightResult==Steinberg::kResultOk);
    CHECK(completionOf(inFlight).revision==1&&completionOf(inFlight).result==Steinberg::kResultOk);
    inFlightCache.update(2,{{61,.5f}});entered=false;release=false;
    std::thread second([&]{inFlightResult=deliver(inFlightCache,inFlight,data,[&](auto&){entered.store(true,std::memory_order_release);while(!release.load(std::memory_order_acquire))std::this_thread::yield();return Steinberg::kResultFalse;});});
    while(!entered.load(std::memory_order_acquire))std::this_thread::yield();
    during=completionOf(inFlight);attempts=inFlight.last.load();
    release.store(true,std::memory_order_release);second.join();
    CHECK(attempts==2&&during.revision==1&&during.result==Steinberg::kResultOk);
    CHECK(inFlightResult==Steinberg::kResultFalse&&completionOf(inFlight).revision==2&&completionOf(inFlight).result==Steinberg::kResultFalse);
    inFlightCache.update(3,{{61,.75f}});threw=false;
    try{deliver(inFlightCache,inFlight,data,[](auto&)->Steinberg::tresult{throw 1;});}catch(int){threw=true;}
    CHECK(threw&&inFlight.last.load()==3&&completionOf(inFlight).revision==2&&data.inputParameterChanges==&hostQueue);
    passedHost=false;deliver(inFlightCache,inFlight,data,hostOriginal);
    CHECK(passedHost&&inFlight.submissions.load()==3&&completionOf(inFlight).revision==2);

    // A torn published batch would expose unequal values, even with individually atomic fields.
    Cache concurrent;std::atomic<bool> finished=false;std::atomic<int> torn=0;
    std::thread writer([&]{for(unsigned n=1;n<=20000;++n){float v=(n%100)/100.f;concurrent.update(n,{{10,v},{11,v}});}finished=true;});
    while(!finished){Snapshot s;if(concurrent.read(s)&&s.count==2&&s.entries[0].value!=s.entries[1].value)++torn;}
    writer.join();CHECK(torn==0);
    Cache bounded;for(unsigned n=0;n<MaxParameters;++n)bounded.update(n+1,{{n,.5f}});
    bool full=false;try{bounded.update(10000,{{999999,.7f}});}catch(const std::length_error&){full=true;}
    CHECK(full&&bounded.read(snapshot)&&snapshot.count==MaxParameters);
    bounded.clear();bounded.update(10001,{{999999,.7f}});CHECK(bounded.read(snapshot)&&snapshot.count==1);
    std::cout<<checks<<" direct delivery checks passed\n";return 0;
}
#endif
