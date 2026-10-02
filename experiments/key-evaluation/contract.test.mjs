import test from 'node:test';
import assert from 'node:assert/strict';
import { key, SKEY_LABELS, manifestItems, windows, monoWindow, replay, evidence } from './contract.mjs';

test('pitch spelling and mode are normalized without guessing unknown modes',()=>{
  assert.deepEqual(key('Db','major'),{pitchClass:1,mode:'major'});
  assert.deepEqual(key('B♭ minor'),{pitchClass:10,mode:'minor'});
  assert.deepEqual(key('C# Major'),{pitchClass:1,mode:'major'});
  assert.throws(()=>key('H Major'));
  assert.throws(()=>key('C Dorian'));
});
test('all 24 S-KEY labels retain upstream major and unusual minor order',()=>{
  assert.deepEqual(SKEY_LABELS.map(x=>key(x).pitchClass),[9,10,11,0,1,2,3,4,5,6,7,8,11,0,1,2,3,4,5,6,7,8,9,10]);
  assert(SKEY_LABELS.slice(0,12).every(x=>key(x).mode==='major'));
  assert(SKEY_LABELS.slice(12).every(x=>key(x).mode==='minor'));
});
test('labels require independent provenance and unknown truth stays unknown',()=>{
  assert.equal(manifestItems([{id:'one',path:'one.wav'}])[0].expected,null);
  assert.throws(()=>manifestItems([{id:'one',path:'one.wav',expected:{pitchClass:0,mode:'major'}}]));
  assert.throws(()=>manifestItems([{id:'one',path:'one.wav',expected:{pitchClass:12,mode:'major'},labelSource:'score'}]));
  assert.throws(()=>manifestItems([{id:'one',path:'one.wav',url:'https://example.com'}]));
  for(const expected of [false,0,''])assert.throws(()=>manifestItems([{id:'one',path:'one.wav',expected,labelSource:'score'}]));
  assert.equal(manifestItems([{id:'one',path:'one.wav',expected:{pitchClass:0,mode:'major'},labelSource:'score'}])[0].expected.pitchClass,0);
});
test('identical endpoints, bounded data and nonfinite rejection',()=>{
  const pcm=new Float32Array(12*96000);
  assert.deepEqual(windows(pcm).map(w=>[w.startFrame,w.endFrame]),[[0,288000],[0,336000],[0,384000],[48000,432000],[96000,480000],[144000,528000],[192000,576000]]);
  assert.throws(()=>windows(new Float32Array(5*96000)));
  assert.throws(()=>windows(new Float32Array(33*96000)));
  assert.throws(()=>windows(new Float32Array(6*96000+1)));
  pcm[0]=NaN; assert.throws(()=>windows(pcm));
});
test('anti-phase and quiet normalization match Rust policy',()=>{
  const a=new Float32Array(6*96000);for(let i=0;i<a.length;i+=2){a[i]=.001;a[i+1]=-.001;}
  const p=monoWindow(a); assert.equal(p.mix,'left');assert.equal(p.gain,64);assert(p.eligible);assert(Math.abs(p.pcm[0]-.064)<1e-7);
  a.fill(0);assert.equal(monoWindow(a).eligible,false);
  a.fill(.2);const n=monoWindow(a);assert.equal(n.mix,'average');assert.equal(n.gain,1);
});
test('initial 3/replacement 5 votes and nulls preserve confirmation semantics',()=>{
  const c=key('C major'), a=key('A minor');
  const rows=[c,c,c,a,a,null,a,a,a,a,a].map((candidate,i)=>({second:i+6,candidate}));
  const r=replay(rows,c);assert.equal(r.firstConfirmedSignalSecond,8);assert.equal(r.firstCorrectSignalSecond,8);
  assert.deepEqual(r.finalConfirmed,a);assert.equal(r.finalCorrect,false);
  assert.equal(replay(rows,null).finalCorrect,null);
  assert.equal(replay([{second:6,candidate:c},{second:7,candidate:null},{second:8,candidate:c}],null).firstConfirmedSignalSecond,null);
});
test('evidence identifies unrelated score scales without calibrated confidence',()=>{
  assert.equal(evidence('essentia',{strength:.7}).scoreKind,'essentia_strength');
  assert.equal(evidence('skey',{scores:Array(24).fill(1/24)}).scoreKind,'model_softmax');
  assert.throws(()=>evidence('skey',{scores:[1]}));
  assert.throws(()=>evidence('essentia',{strength:NaN}));
});
