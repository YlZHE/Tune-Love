import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {manifestItems, RATE} from './contract.mjs';
import {synthetic} from './fixtures.mjs';

const demuxers=new Map([
  ['.wav','wav'],['.wave','wav'],['.flac','flac'],['.mp3','mp3'],
  ['.ogg','ogg'],['.oga','ogg'],['.aac','aac'],['.aif','aiff'],['.aiff','aiff']
]);
function hasSignature(type,bytes){
  const ascii=(start,end)=>bytes.toString('ascii',start,end);
  if(type==='wav')return ascii(0,4)==='RIFF'&&ascii(8,12)==='WAVE';
  if(type==='flac')return ascii(0,4)==='fLaC';
  if(type==='ogg')return ascii(0,4)==='OggS';
  if(type==='aiff')return ascii(0,4)==='FORM'&&['AIFF','AIFC'].includes(ascii(8,12));
  if(type==='aac')return bytes.length>=2&&bytes[0]===0xff&&(bytes[1]&0xf6)===0xf0;
  if(type==='mp3')return ascii(0,3)==='ID3'||(bytes.length>=2&&bytes[0]===0xff&&(bytes[1]&0xe0)===0xe0);
  return false;
}
export function parseArgs(args){
  let mode=null,manifest=null,output=null,repeats=3,nativeOnly=false,nativeMode='incremental',probe=null;
  const used=new Set();
  for(let i=0;i<args.length;i++){
    const a=args[i];if(used.has(a))throw new Error(`Duplicate option ${a}`);used.add(a);
    if(a==='--synthetic'){if(mode)throw new Error('Choose synthetic OR manifest');mode='synthetic';}
    else if(a==='--native-only')nativeOnly=true;
    else if(['--manifest','--output','--repeat','--probe','--native-mode'].includes(a)){
      const value=args[++i];if(value==null||value.startsWith('--'))throw new Error(`Missing value for ${a}`);
      if(a==='--manifest'){if(mode)throw new Error('Choose synthetic OR manifest');mode='manifest';manifest=path.resolve(value);}
      if(a==='--output')output=path.resolve(value);
      if(a==='--repeat')repeats=Number(value);
      if(a==='--probe')probe=path.resolve(value);
      if(a==='--native-mode')nativeMode=value;
    }else throw new Error(`Unknown option ${a}`);
  }
  if(!['incremental','batch'].includes(nativeMode))throw new Error('Invalid native mode; use incremental or batch');
  if(!mode||!output||!Number.isInteger(repeats)||repeats<1||repeats>10)throw new Error('Usage: compare.mjs --synthetic | --manifest file.json --output result.json [--repeat 1..10] [--native-mode incremental|batch] [--probe file.exe] [--native-only]');
  if(fs.existsSync(output))throw new Error('Output already exists; preserve previous evidence with a new filename');
  return {mode,manifest,output,repeats,nativeOnly,nativeMode,probe};
}
export function decodeLocal(filename){
  if(/^\\\\|^\/\/|^[a-z]+:\/\//i.test(filename))throw new Error('Only explicit local files are allowed');
  const resolved=fs.realpathSync(filename);
  if(/^\\\\|^\/\//.test(resolved)||!fs.statSync(resolved).isFile())throw new Error('Audio must be a local regular file');
  const type=demuxers.get(path.extname(resolved).toLowerCase());
  if(!type)throw new Error('Unsupported audio extension; playlists and containers with external references are excluded');
  const fd=fs.openSync(resolved,'r');let bytes;
  try{bytes=Buffer.alloc(12);bytes=bytes.subarray(0,fs.readSync(fd,bytes,0,12,0));}
  finally{fs.closeSync(fd);}
  if(!hasSignature(type,bytes))throw new Error('Unsupported audio signature for explicit demuxer');
  const buffer=execFileSync('ffmpeg',['-nostdin','-v','error','-protocol_whitelist','file,pipe','-f',type,'-i',resolved,'-t','32','-vn','-f','f32le','-acodec','pcm_f32le','-ar','48000','-ac','2','pipe:1'],{windowsHide:true,maxBuffer:32*RATE*8+1048576,timeout:120000});
  if(buffer.length%8||buffer.length>32*RATE*8)throw new Error('Invalid decoder output');
  const pcm=new Float32Array(buffer.length/4);for(let i=0;i<pcm.length;i++)pcm[i]=buffer.readFloatLE(i*4);
  return pcm;
}
export function* caseItems(config,decode=decodeLocal){
  if(config.mode==='synthetic'){yield* synthetic();return;}
  if(fs.statSync(config.manifest).size>1048576)throw new Error('Manifest exceeds 1 MiB');
  const items=manifestItems(JSON.parse(fs.readFileSync(config.manifest,'utf8').replace(/^\uFEFF/,'')));
  for(const item of items)yield {...item,kind:'local-file',pcm:decode(path.resolve(path.dirname(config.manifest),item.path))};
}
export function wireId(caseIndex,repeat,windowIndex){
  return `c${caseIndex}r${repeat}w${windowIndex}`;
}
