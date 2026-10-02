import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {execFileSync} from 'node:child_process';
import {parseArgs, decodeLocal, caseItems, wireId} from './input.mjs';

test('native mode defaults to incremental and accepts only batch or incremental',()=>{
  const base=['--synthetic','--output',path.join(os.tmpdir(),'key-lab-unused-output.json')];
  assert.equal(parseArgs(base).nativeMode,'incremental');
  assert.equal(parseArgs([...base,'--native-mode','batch']).nativeMode,'batch');
  assert.throws(()=>parseArgs([...base,'--native-mode','other']),/native mode/i);
});

test('explicit demuxer rejects playlists and disguised playlist bytes before decoding',()=>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'key-lab-input-'));
  try{
    for(const name of ['list.m3u','list.ffcat','list.wav']){
      const file=path.join(dir,name);
      fs.writeFileSync(file,name.endsWith('ffcat')?'ffconcat version 1.0\nfile other.wav\n':'#EXTM3U\nother.wav\n');
      assert.throws(()=>decodeLocal(file),/unsupported|signature|audio/i);
    }
  }finally{fs.rmSync(dir,{recursive:true,force:true});}
});

test('manifest iterator decodes only a requested case, independent of remaining metadata',()=>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'key-lab-iterate-'));
  try{
    const manifest=path.join(dir,'cases.json');
    fs.writeFileSync(manifest,JSON.stringify([{id:'one',path:'a.wav'},{id:'two',path:'b.wav'}]));
    const decoded=[];
    const iterator=caseItems({mode:'manifest',manifest},file=>{decoded.push(path.basename(file));return new Float32Array(6*48000*2);});
    assert.deepEqual(decoded,[]);
    assert.equal(iterator.next().value.id,'one');
    assert.deepEqual(decoded,['a.wav']);
    assert.equal(iterator.next().value.id,'two');
    assert.deepEqual(decoded,['a.wav','b.wav']);
  }finally{fs.rmSync(dir,{recursive:true,force:true});}
});

test('wire IDs remain bounded independently of accepted display IDs',()=>{
  const display='long'.repeat(100000);
  assert(display.length>128);
  assert.equal(wireId(99,9,6),'c99r9w6');
  assert(wireId(99,9,6).length<=128);
});

test('one-case CLI passes batch to the saved native probe and labels every row cold',()=>{
  const root=path.resolve(path.dirname(fileURLToPath(import.meta.url)),'../..');
  const dir=path.join(root,'artifacts/key-engine-evaluation');
  const stem=`task-2-fix-batch-${Date.now()}-${process.pid}`;
  const wav=path.join(dir,`${stem}.wav`),manifest=path.join(dir,`${stem}.json`),output=path.join(dir,`${stem}-result.json`);
  const frames=6*48000,bytes=Buffer.alloc(44+frames*2*4);
  bytes.write('RIFF',0);bytes.writeUInt32LE(bytes.length-8,4);bytes.write('WAVEfmt ',8);
  bytes.writeUInt32LE(16,16);bytes.writeUInt16LE(3,20);bytes.writeUInt16LE(2,22);
  bytes.writeUInt32LE(48000,24);bytes.writeUInt32LE(48000*8,28);
  bytes.writeUInt16LE(8,32);bytes.writeUInt16LE(32,34);bytes.write('data',36);
  bytes.writeUInt32LE(frames*8,40);
  fs.writeFileSync(wav,bytes,{flag:'wx'});
  fs.writeFileSync(manifest,JSON.stringify([{id:'case-id-'.repeat(30),path:path.basename(wav)}]),{flag:'wx'});
  execFileSync(process.execPath,[path.join(root,'experiments/key-evaluation/compare.mjs'),'--manifest',manifest,'--native-only','--native-mode','batch','--repeat','1','--output',output],{cwd:root,windowsHide:true,timeout:120000,maxBuffer:1048576});
  const result=JSON.parse(fs.readFileSync(output,'utf8'));
  assert.equal(result.nativeMode,'batch');
  assert.equal(result.cases[0].id,'case-id-'.repeat(30));
  assert.equal(result.cases[0].runs[0].engines.native.rows.length,1);
  assert.equal(result.summary.native.coldContextTiming.count,1);
  assert.equal(result.summary.native.warmTiming,null);
});
