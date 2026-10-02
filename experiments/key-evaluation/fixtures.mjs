import {RATE} from './contract.mjs';
const SECONDS=12;
function cadence(root,minor,amplitude,anti=false){
  const chords=minor?[[0,3,7],[5,8,0],[7,11,2],[0,3,7]]:[[0,4,7],[5,9,0],[7,11,2],[0,4,7]];
  const pcm=new Float32Array(SECONDS*RATE*2);
  for(let frame=0;frame<SECONDS*RATE;frame++){
    const local=frame%96000,env=Math.min(1,Math.min(local,96000-local-1)/480),t=frame/RATE;
    let sample=0;for(const interval of chords[Math.floor(frame/96000)%4]){
      const f=440*2**((48+(root+interval)%12-69)/12);
      sample+=Math.sin(2*Math.PI*f*t)+.2*Math.sin(4*Math.PI*f*t);
    }
    const value=Math.fround(Math.fround(sample*env/3.6)*Math.fround(amplitude));
    pcm[frame*2]=value;pcm[frame*2+1]=anti?-value:value;
  }return pcm;
}
export function synthetic(){
  const result=[["c-major",0,false,.65,false],["a-minor",9,true,.65,false],['quiet-c-major',0,false,.00065,false],['antiphase-c-major',0,false,.65,true]].map(([id,root,minor,level,anti])=>({
    id,kind:'synthetic-cadence',expected:{pitchClass:root,mode:minor?'minor':'major'},labelSource:'constructed tonic/subdominant/dominant cadence; not real-song ground truth',pcm:cadence(root,minor,level,anti)}));
  let seed=0x2468abcd;
  for(const id of ['silence','single-tone','noise','clicks']){
    const pcm=new Float32Array(SECONDS*RATE*2);
    for(let i=0;i<SECONDS*RATE;i++){
      seed=(Math.imul(seed,1664525)+1013904223)>>>0;
      const v=id==='silence'?0:id==='single-tone'?.3*Math.sin(2*Math.PI*440*i/RATE):id==='noise'?(seed/4294967296*2-1)*.12:(i%12000<96?.65*Math.exp(-(i%12000)/12)*(i%2?1:-1):0);
      pcm[2*i]=v;pcm[2*i+1]=v;
    }result.push({id,kind:'synthetic-negative',expected:null,labelSource:null,pcm});
  }
  return result;
}
