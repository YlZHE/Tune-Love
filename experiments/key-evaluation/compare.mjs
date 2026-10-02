import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import {fileURLToPath} from 'node:url';
import {execFileSync} from 'node:child_process';
import {performance} from 'node:perf_hooks';
import {key,windows,monoWindow,replay,evidence,stats} from './contract.mjs';
import {parseArgs,caseItems,wireId} from './input.mjs';
import {startWorker} from './worker-client.mjs';

const HERE=path.dirname(fileURLToPath(import.meta.url)),ROOT=path.resolve(HERE,'../..');
function nativeRun(probe,mode,pcm){
  const start=performance.now();
  const bytes=Buffer.from(pcm.buffer,pcm.byteOffset,pcm.byteLength);
  const output=execFileSync(probe,['--mode',mode],{input:bytes,windowsHide:true,maxBuffer:1048576,timeout:120000});
  const result=JSON.parse(output.toString());
  return {...result,processWallMs:performance.now()-start};
}
function assertBounds(rows,shared){
  if(rows.length!==shared.length)throw new Error('Engine row count differs');
  rows.forEach((r,i)=>{if(r.startFrame!==shared[i].startFrame||r.endFrame!==shared[i].endFrame)throw new Error('Engine windows differ');});
}
async function run(config){
  if(os.endianness()!=='LE')throw new Error('This lab requires little-endian PCM');
  const runtimes={},workers={};
  const report={schemaVersion:1,createdAt:new Date().toISOString(),runtime:{node:process.version,platform:os.platform(),release:os.release(),cpu:os.cpus()[0]?.model},
    constraints:{liveAudio:false,audioStored:false,inferenceNetwork:false,mainAppLaunchedByRunner:false,realSongAccuracy:null},
    sharedInput:{sampleRate:48000,sourceChannels:2,candidateChannels:1,endpoints:'6..12 seconds, trailing <=8 seconds',
      precision:'Rust mono f64; WASM/model mono f32',normalization:'strongest channel if average RMS < 25%; RMS floor 5e-5; target .10; gain 1..64; clamp [-1,1]'},
    timingNotes:`Serial, no compilation. Worker totals exclude shared JS downmix, pipe/base64 and validation; S-KEY includes 48k->22.05k resample. Native elapsed includes Rust downmix and optional scores. ${config.nativeMode==='batch'?'Batch reconstructs context for every row; no warm incremental context timing is reported.':'Native first row per process is cold context; subsequent rows reuse incremental context.'} Worker warmup is recorded separately.`,
    nativeMode:config.nativeMode,repeats:config.repeats,runtimes,cases:[],summary:{}};
  try{
    if(!config.nativeOnly){
      workers.essentia=await startWorker(process.execPath,[path.join(HERE,'essentia-worker.cjs')],{cwd:ROOT});
      runtimes.essentia=workers.essentia.ready;
      workers.skey=await startWorker(path.join(ROOT,'artifacts/key-engine-evaluation/venv/Scripts/python.exe'),['-B',path.join(HERE,'skey-worker.py')],{cwd:ROOT});
      runtimes.skey=workers.skey.ready;
    }
    let caseIndex=0;
    for(const item of caseItems(config)){
      const shared=windows(item.pcm),preprocessed=shared.map(w=>monoWindow(w.pcm));
      if(!config.nativeOnly&&caseIndex===0){
        const warm=preprocessed[0].pcm;
        for(const [engine,worker] of Object.entries(workers))runtimes[engine].coldWarmup=await worker.request('warmup',warm);
      }
      const result={id:item.id,kind:item.kind,labelSource:item.labelSource,expected:item.expected,runs:[]};
      for(let repeat=0;repeat<config.repeats;repeat++){
        const native=nativeRun(config.probe??path.join(ROOT,'src-tauri/target/debug/examples/key_window_probe.exe'),config.nativeMode,item.pcm);assertBounds(native.rows,shared);
        if(native.mode!==config.nativeMode)throw new Error('Probe returned a different native mode');
        if(runtimes.native&&JSON.stringify(runtimes.native)!==JSON.stringify(native.build))throw new Error('Probe build identity changed mid-run');
        runtimes.native=native.build;
        const engines={native:{rows:native.rows,processWallMs:native.processWallMs,confirmation:replay(native.rows,item.expected)}};
        for(const [engine,worker] of Object.entries(workers)){
          const rows=[];
          for(let i=0;i<shared.length;i++){
            const w=shared[i],p=preprocessed[i],raw=await worker.request(wireId(caseIndex,repeat,i),p.pcm);
            const rawCandidate=engine==='essentia'?key(raw.raw.key,raw.raw.scale):key(raw.raw.label);
            rows.push({second:w.second,startFrame:w.startFrame,endFrame:w.endFrame,rawCandidate,candidate:p.eligible?rawCandidate:null,
              preprocessing:{mix:p.mix,rms:p.rms,gain:p.gain,eligible:p.eligible,clipped:p.clipped},evidence:evidence(engine,raw.raw),
              preprocessMs:raw.preprocessMs,inferenceMs:raw.inferenceMs,elapsedMs:raw.totalMs});
          }
          assertBounds(rows,shared);engines[engine]={rows,confirmation:replay(rows,item.expected)};
        }
        result.runs.push({repeat,engines});
      }
      report.cases.push(result);process.stderr.write(`Evaluated ${item.id}\n`);
      caseIndex++;
    }
    for(const engine of ['native',...Object.keys(workers)]){
      const runs=report.cases.flatMap(c=>c.runs.map(r=>r.engines[engine]));
      const cold=engine==='native'?runs.flatMap(r=>(config.nativeMode==='batch'?r.rows:r.rows.slice(0,1)).map(x=>x.elapsedMs)):[];
      const warm=runs.flatMap(r=>(engine==='native'?(config.nativeMode==='batch'?[]:r.rows.slice(1)):r.rows).map(x=>x.elapsedMs));
      const labeled=report.cases.filter(c=>c.expected);
      report.summary[engine]={coldContextTiming:stats(cold),warmTiming:stats(warm),
        labeledSyntheticFinal:report.cases.filter(c=>c.kind==='synthetic-cadence').map(c=>({id:c.id,results:c.runs.map(r=>r.engines[engine].confirmation)})),
        realSongAccuracy:null,labeledCaseCount:labeled.length,unlabeledCaseCount:report.cases.length-labeled.length};
    }
  }finally{
    const errors=[];
    for(const [engine,worker] of Object.entries(workers)){try{runtimes[engine].stderr=await worker.close();}catch(e){errors.push(e);}}
    if(errors.length)throw new AggregateError(errors,'Worker cleanup failure');
  }
  fs.mkdirSync(path.dirname(config.output),{recursive:true});
  fs.writeFileSync(config.output,JSON.stringify(report,null,2)+'\n',{flag:'wx'});
  return {output:config.output,summary:report.summary};
}
try{console.log(JSON.stringify(await run(parseArgs(process.argv.slice(2))),null,2));}
catch(error){console.error(error.stack??error);process.exitCode=1;}
