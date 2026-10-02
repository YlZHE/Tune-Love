// Real dependency smoke/protocol checks; no capture or fabricated engine results.
import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {startWorker} from './worker-client.mjs';
import {evidence,key,monoWindow,windows} from './contract.mjs';
import {synthetic} from './fixtures.mjs';
const here=path.dirname(fileURLToPath(import.meta.url)),root=path.resolve(here,'../..');

for(const engine of ['essentia','skey'])test(`${engine}: real inference and invalid PCM fail closed`,{timeout:90000},async()=>{
  const command=engine==='essentia'?process.execPath:path.join(root,'artifacts/key-engine-evaluation/venv/Scripts/python.exe');
  const args=engine==='essentia'?[path.join(here,'essentia-worker.cjs')]:['-B',path.join(here,'skey-worker.py')];
  const worker=await startWorker(command,args,{cwd:root});
  try{
    assert(worker.ready.loadMs>=0);
    const pcm=monoWindow(windows(synthetic()[0].pcm)[0].pcm).pcm;
    const result=await worker.request('cadence',pcm);
    const candidate=engine==='essentia'?key(result.raw.key,result.raw.scale):key(result.raw.label);
    assert.deepEqual(candidate,{pitchClass:0,mode:'major'});
    assert(evidence(engine,result.raw));assert(result.totalMs>=0);
    await assert.rejects(worker.request('too-short',new Float32Array(8)),/size/i);
    pcm[0]=NaN;await assert.rejects(worker.request('nonfinite',pcm),/nonfinite/i);
    pcm.fill(0);const silence=await worker.request('silence',pcm);
    assert(silence.raw); // retain any raw guess; shared gate belongs to controller
  }finally{await worker.close();}
});
