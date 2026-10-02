export const RATE = 48000;
export const SKEY_LABELS = ['A Major','Bb Major','B Major','C Major','C# Major','D Major','D# Major','E Major','F Major','F# Major','G Major','G# Major','B minor','C minor','C# minor','D minor','D# minor','E minor','F minor','F# minor','G minor','G# minor','A minor','Bb minor'];
const pitches = {C:0,D:2,E:4,F:5,G:7,A:9,B:11};
export function key(name, scale) {
  const text = `${name}${scale ? ` ${scale}` : ''}`.replaceAll('♭','b').replaceAll('♯','#').trim();
  const m = /^([A-G])([b#]?)\s+(major|minor)$/i.exec(text);
  if (!m) throw new Error(`Unsupported key label: ${text}`);
  return {pitchClass:(pitches[m[1].toUpperCase()] + (m[2]==='#'?1:m[2]==='b'?-1:0)+12)%12,mode:m[3].toLowerCase()};
}
export function same(a,b) { return a!=null && b!=null && a.pitchClass===b.pitchClass && a.mode===b.mode; }
export function manifestItems(items) {
  if (!Array.isArray(items) || items.length<1 || items.length>100) throw new Error('Manifest requires 1..100 items');
  const ids = new Set();
  return items.map(x=>{
    if (!x || typeof x!=='object' || Object.keys(x).some(k=>!['id','path','expected','labelSource'].includes(k))) throw new Error('Unexpected manifest field');
    if (typeof x.id!=='string'||!x.id.trim()||ids.has(x.id)||typeof x.path!=='string'||!x.path.trim()) throw new Error('Unique id and local path required');
    ids.add(x.id);
    const e=x.expected??null;
    if(e!==null && (typeof e!=='object'||Array.isArray(e)))throw new Error('Expected key must be an object or null');
    if(e && (!Number.isInteger(e.pitchClass)||e.pitchClass<0||e.pitchClass>11||!['major','minor'].includes(e.mode)||Object.keys(e).some(k=>!['pitchClass','mode'].includes(k)))) throw new Error('Invalid expected key');
    if(e && (typeof x.labelSource!=='string'||!x.labelSource.trim())) throw new Error('Expected key requires independent labelSource');
    return {...x,expected:e,labelSource:x.labelSource??null};
  });
}
export function validPcm(pcm,maxSeconds=32,channels=2) {
  if(!(pcm instanceof Float32Array)||pcm.length%channels!==0||pcm.length<6*RATE*channels||pcm.length>maxSeconds*RATE*channels) throw new Error('Invalid PCM shape/length');
  if(pcm.some(x=>!Number.isFinite(x))) throw new Error('Nonfinite PCM');
}
export function windows(pcm,end=12) {
  validPcm(pcm);
  if(!Number.isInteger(end)||end<6||end>12) throw new Error('End must be 6..12');
  return Array.from({length:Math.min(end,Math.floor(pcm.length/(2*RATE)))-5},(_,i)=>{
    const second=i+6,endFrame=second*RATE,startFrame=Math.max(0,endFrame-8*RATE);
    return {second,startFrame,endFrame,pcm:pcm.subarray(startFrame*2,endFrame*2)};
  });
}
export function monoWindow(input) {
  validPcm(input,8);
  const frames=input.length/2;let l=0,r=0,a=0;
  for(let i=0;i<input.length;i+=2){l+=input[i]**2;r+=input[i+1]**2;a+=((input[i]+input[i+1])/2)**2;}
  l=Math.sqrt(l/frames);r=Math.sqrt(r/frames);a=Math.sqrt(a/frames);
  const mix=Math.max(l,r)>0 && a<Math.max(l,r)*.25 ? (r>l?'right':'left'):'average';
  const rms=mix==='left'?l:mix==='right'?r:a,eligible=rms>5e-5;
  const gain=eligible?Math.min(64,Math.max(1,.1/rms)):1;
  let clipped=false;const pcm=new Float32Array(frames);
  for(let i=0;i<frames;i++){
    const v=(mix==='left'?input[2*i]:mix==='right'?input[2*i+1]:(input[2*i]+input[2*i+1])/2)*gain;
    clipped ||= Math.abs(v)>1;pcm[i]=Math.min(1,Math.max(-1,v));
  }
  return {pcm,mix,rms,gain,eligible,clipped};
}
export function evidence(engine, raw) {
  if(engine==='essentia'){
    if(!Number.isFinite(raw.strength)) throw new Error('Invalid Essentia strength');
    return {scoreKind:'essentia_strength',strength:raw.strength,calibrated:false};
  }
  if(engine==='skey'){
    if(!Array.isArray(raw.scores)||raw.scores.length!==24||raw.scores.some(x=>!Number.isFinite(x))) throw new Error('Invalid S-KEY scores');
    return {scoreKind:'model_softmax',scores:raw.scores.map((score,i)=>({key:key(SKEY_LABELS[i]),score})),calibrated:false};
  }
  throw new Error('Unknown score kind');
}
export function replay(rows,expected) {
  let confirmed=null,pending=null,votes=0,first=null,correct=null,flips=0;
  for(const row of rows){
    const c=row.candidate;
    if(c==null || same(c,confirmed)){pending=null;votes=0;continue;}
    if(same(c,pending))votes++;else{pending=c;votes=1;}
    if(votes >= (confirmed?5:3)) {
      if(confirmed)flips++;
      confirmed=c;pending=null;votes=0;first??=row.second;
      if(same(confirmed,expected))correct??=row.second;
    }
  }
  return {firstConfirmedSignalSecond:first,firstCorrectSignalSecond:expected?correct:null,finalConfirmed:confirmed,
    finalCorrect:expected?same(confirmed,expected):null,confirmedFlips:flips,kind:'offline 3/5 consecutive-vote replay'};
}
export function stats(values){
  if(!values.length)return null;
  const v=[...values].sort((a,b)=>a-b);
  return {count:v.length,meanMs:v.reduce((a,b)=>a+b,0)/v.length,medianMs:v.length%2?v[(v.length-1)/2]:(v[v.length/2-1]+v[v.length/2])/2,p95Ms:v[Math.ceil(v.length*.95)-1],maxMs:v.at(-1)};
}
