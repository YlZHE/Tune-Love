// Offline worker. One bounded mono float32/base64 request per JSON line.
const path=require('node:path');
const {performance}=require('node:perf_hooks');
const start=performance.now();
const root=path.resolve(__dirname,'../..');
global.fetch=()=>Promise.reject(new Error('Network disabled in evaluation worker'));
const {Essentia,EssentiaWASM}=require(path.join(root,'artifacts/key-engine-evaluation/essentia-runtime/node_modules/essentia.js'));
const core=new Essentia(EssentiaWASM);
const emit=x=>process.stdout.write(JSON.stringify(x)+'\n');
const args=[true,4096,4096,12,3500,60,25,.2,'bgate',48000,.0001,440,'cosine','hann'];
emit({type:'ready',engine:'Essentia WASM',version:'0.1.3',loadMs:performance.now()-start,keyExtractorArgs:args});
let pending='';
process.stdin.setEncoding('utf8');
process.stdin.on('data',chunk=>{
  pending+=chunk;
  if(pending.length>2200000){process.stderr.write('oversized request\n');process.exitCode=1;process.stdin.destroy();return;}
  let newline;
  while((newline=pending.indexOf('\n'))>=0){
    const line=pending.slice(0,newline);pending=pending.slice(newline+1);let q;
    try{
      q=JSON.parse(line);
      if(typeof q.id!=='string'||q.id.length>128||q.sampleRate!==48000||q.channels!==1||typeof q.pcm!=='string')throw new Error('invalid request metadata');
      const bytes=Buffer.from(q.pcm,'base64');
      if(bytes.toString('base64')!==q.pcm||bytes.length%4||bytes.length<6*48000*4||bytes.length>8*48000*4)throw new Error('invalid PCM encoding/size');
      const audio=new Float32Array(bytes.length/4);
      for(let i=0;i<audio.length;i++){audio[i]=bytes.readFloatLE(i*4);if(!Number.isFinite(audio[i]))throw new Error('nonfinite PCM');}
      let vector;
      const begin=performance.now();
      try{
        vector=core.arrayToVector(audio);
        const feature=performance.now();
        const result=core.KeyExtractor(vector,...args);
        const end=performance.now();
        emit({id:q.id,raw:{key:result.key,scale:result.scale,strength:result.strength},preprocessMs:feature-begin,inferenceMs:end-feature,totalMs:end-begin});
      }finally{if(vector)vector.delete();}
    }catch(e){emit({id:q?.id??null,error:String(e?.stack??e)});}
  }
});
process.stdin.on('end',()=>{if(pending.trim()){process.stderr.write('truncated request\n');process.exitCode=1;}});
