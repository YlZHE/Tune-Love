// Independent implementation of the observed cache and process-delivery semantics.
#pragma once
#include <array>
#include <atomic>
#include <bit>
#include <cmath>
#include <cstdint>
#include <initializer_list>
#include <mutex>
#include <span>
#include <stdexcept>
#include <thread>
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"

namespace atr {
constexpr unsigned MaxParameters=256;
struct Entry { uint32_t id=0; float value=0; };
struct Snapshot { uint64_t serial=0; unsigned count=0; std::array<Entry,MaxParameters> entries{}; };
enum class Update { Applied, Duplicate };

class Cache {
    std::mutex writers;
    std::atomic_flag gate=ATOMIC_FLAG_INIT;
    Snapshot current{};
    uint64_t external=0;
    bool received=false;
    struct Unlock { std::atomic_flag& gate; ~Unlock(){gate.clear(std::memory_order_release);} };
public:
    Update update(uint64_t serial,std::span<const Entry> values,uint64_t* committedRevision=nullptr){
        if(values.empty()||values.size()>MaxParameters)throw std::invalid_argument("invalid batch size");
        for(const auto& v:values)if(!std::isfinite(v.value)||v.value<0||v.value>1)
            throw std::invalid_argument("value must be finite normalized 0..1");
        std::lock_guard lock(writers);
        if(received&&serial==external){if(committedRevision)*committedRevision=current.serial;return Update::Duplicate;}
        // Writers may wait. Readers (the audio callback) only try once.
        while(gate.test_and_set(std::memory_order_acquire))std::this_thread::yield();
        Unlock unlock{gate};
        Snapshot next=current;
        for(const auto& v:values){
            unsigned i=0;while(i<next.count&&next.entries[i].id!=v.id)++i;
            if(i==next.count){if(i==MaxParameters)throw std::length_error("parameter cache full");++next.count;}
            next.entries[i]=v;
        }
        // Internal revision is independent of the message's duplicate token.
        next.serial=current.serial==0x7fffffff?1:current.serial+1;
        current=next;external=serial;received=true;if(committedRevision)*committedRevision=current.serial;return Update::Applied;
    }
    Update update(uint64_t serial,std::initializer_list<Entry> values){return update(serial,std::span(values.begin(),values.size()));}
    bool read(Snapshot& out) noexcept {
        if(gate.test_and_set(std::memory_order_acquire))return false;
        out=current;gate.clear(std::memory_order_release);return true;
    }
    void clear(){
        std::lock_guard lock(writers);
        while(gate.test_and_set(std::memory_order_acquire))std::this_thread::yield();
        current.count=0;gate.clear(std::memory_order_release);
    }
};

using namespace Steinberg;
using namespace Steinberg::Vst;
class Queue final:public IParamValueQueue {
public:
    Entry entry{};
    tresult PLUGIN_API queryInterface(const TUID requested,void** out) override {
        if(!out)return kInvalidArgument;*out=nullptr;
        if(!std::memcmp(requested,INLINE_UID_OF(IParamValueQueue),16)||!std::memcmp(requested,INLINE_UID_OF(FUnknown),16)){
            *out=static_cast<IParamValueQueue*>(this);return kResultOk;
        }return kNoInterface;
    }
    uint32 PLUGIN_API addRef() override{return 1;}
    uint32 PLUGIN_API release() override{return 1;}
    ParamID PLUGIN_API getParameterId() override{return entry.id;}
    int32 PLUGIN_API getPointCount() override{return 1;}
    tresult PLUGIN_API getPoint(int32 index,int32& offset,ParamValue& value) override{
        if(index!=0)return kInvalidArgument;offset=0;value=entry.value;return kResultOk;
    }
    tresult PLUGIN_API addPoint(int32,ParamValue,int32&) override{return kNotImplemented;}
};
class Changes final:public IParameterChanges {
    unsigned count;
    std::array<Queue,MaxParameters> queues;
public:
    explicit Changes(const Snapshot& s):count(s.count){for(unsigned i=0;i<count;++i)queues[i].entry=s.entries[i];}
    tresult PLUGIN_API queryInterface(const TUID requested,void** out) override{
        if(!out)return kInvalidArgument;*out=nullptr;
        if(!std::memcmp(requested,INLINE_UID_OF(IParameterChanges),16)||!std::memcmp(requested,INLINE_UID_OF(FUnknown),16)){
            *out=static_cast<IParameterChanges*>(this);return kResultOk;
        }return kNoInterface;
    }
    uint32 PLUGIN_API addRef() override{return 1;}
    uint32 PLUGIN_API release() override{return 1;}
    int32 PLUGIN_API getParameterCount() override{return static_cast<int32>(count);}
    IParamValueQueue* PLUGIN_API getParameterData(int32 i) override{return i>=0&&static_cast<unsigned>(i)<count?&queues[i]:nullptr;}
    IParamValueQueue* PLUGIN_API addParameterData(const ParamID&,int32&) override{return nullptr;}
};
struct Consumer {
    std::atomic<uint64_t> last{0},submissions{0};
    std::atomic<tresult> processResult{kNotImplemented};
    struct Completion {uint64_t revision;tresult result;};
    // One atomic word prevents a result from a newer call being paired with
    // an older revision. Cache revisions occupy at most 31 bits.
    static_assert(sizeof(tresult)==4&&std::atomic<uint64_t>::is_always_lock_free);
private:
    std::atomic<uint64_t> completed{static_cast<uint32_t>(kNotImplemented)};
public:
    void publishCompletion(uint64_t revision,tresult result) noexcept {
        completed.store((revision<<32)|static_cast<uint32_t>(result),std::memory_order_release);
    }
    Completion completion() const noexcept {
        const auto word=completed.load(std::memory_order_acquire);
        return {word>>32,std::bit_cast<tresult>(static_cast<uint32_t>(word))};
    }
};
template<class Fn> tresult deliver(Cache& cache,Consumer& consumer,ProcessData& data,Fn original){
    Snapshot snapshot;
    if(!cache.read(snapshot)||snapshot.serial==consumer.last.load(std::memory_order_relaxed))return original(data);
    consumer.last.store(snapshot.serial,std::memory_order_relaxed);
    if(!snapshot.count)return original(data);
    Changes changes(snapshot);
    struct Restore { ProcessData& data;IParameterChanges* saved;~Restore(){data.inputParameterChanges=saved;} } restore{data,data.inputParameterChanges};
    data.inputParameterChanges=&changes;
    consumer.submissions.fetch_add(1,std::memory_order_relaxed);
    auto result=original(data);consumer.processResult.store(result,std::memory_order_relaxed);
    consumer.publishCompletion(snapshot.serial,result);return result;
}
}
