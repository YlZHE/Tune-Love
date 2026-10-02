import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {execFileSync} from 'node:child_process';
import {synthetic} from './fixtures.mjs';
import {windows} from './contract.mjs';
const root=path.resolve(path.dirname(fileURLToPath(import.meta.url)),'../..');
const probe=process.env.KEY_EVALUATION_PROBE??path.join(root,'src-tauri/target/debug/examples/key_window_probe.exe');
function run(args,input){return execFileSync(probe,args,{input,stdio:['pipe','pipe','pipe'],windowsHide:true,timeout:60000,maxBuffer:1048576});}
test('real stdin probe emits identical requested bounds and diagnostic build contract',()=>{
  const pcm=synthetic()[0].pcm,input=Buffer.from(pcm.buffer),shared=windows(pcm);
  const result=JSON.parse(run(['--mode','incremental'],input));
  assert.equal(result.schemaVersion,1);assert.equal(result.mode,'incremental');assert.equal(result.rows.length,7);
  assert.equal(result.build.rustProfile,'debug');assert(['0','2'].includes(result.build.nativeEffective));
  result.rows.forEach((r,i)=>{
    assert.equal(r.startFrame,shared[i].startFrame);assert.equal(r.endFrame,shared[i].endFrame);
    assert(Number.isFinite(r.elapsedMs));assert.equal(r.diagnostics.scores.length,24);
    assert.equal(r.diagnostics.scoreKind,'cosine_similarity');
  });
  const batch=JSON.parse(run(['--mode','batch','--end','6'],input));
  assert.equal(batch.rows.length,1);assert.equal(batch.rows[0].startFrame,0);assert.equal(batch.rows[0].endFrame,288000);
});
test('real stdin probe rejects oversized, short, nonfinite and malformed inputs',()=>{
  for(const input of [Buffer.alloc(32*48000*8+8),Buffer.alloc(10),Buffer.alloc(6*48000*8+1)])assert.throws(()=>run([],input));
  const bad=Buffer.alloc(6*48000*8);bad.writeFloatLE(Infinity,0);assert.throws(()=>run([],bad));
  assert.throws(()=>run(['--end','31'],Buffer.alloc(6*48000*8)));
});
test('long observation reaches real audio end without changing the default horizon or padding short inputs',()=>{
  const audio=Buffer.alloc(30*48000*8);
  const long=JSON.parse(run(['--end','30'],audio));
  assert.equal(long.rows.length,25);assert.equal(long.rows.at(-1).second,30);
  assert.equal(long.rows.at(-1).startFrame,1056000);assert.equal(long.rows.at(-1).endFrame,1440000);
  assert(long.rows.every(r=>r.candidate===null));
  assert.equal(JSON.parse(run([],audio)).rows.length,7);
  const short=JSON.parse(run(['--end','30'],Buffer.alloc(29*48000*8)));
  assert.equal(short.rows.length,24);assert.equal(short.rows.at(-1).second,29);
});
