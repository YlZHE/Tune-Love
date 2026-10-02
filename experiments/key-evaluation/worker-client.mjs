import {spawn} from 'node:child_process';
import {performance} from 'node:perf_hooks';

export async function startWorker(command,args,{cwd,timeoutMs=60000}={}) {
  const launched=performance.now();
  const child=spawn(command,args,{cwd,windowsHide:true,stdio:['pipe','pipe','pipe']});
  let wait=null,buffer='',stderr='',closed=false,failure=null;
  child.stderr.setEncoding('utf8');child.stderr.on('data',s=>{stderr=(stderr+s).slice(-24000);});
  const reject=e=>{failure=e;if(wait){clearTimeout(wait.timer);wait.reject(e);wait=null;}};
  child.on('error',reject);
  child.stdin.on('error',reject);
  const exitPromise=new Promise(resolve=>child.on('close',(code)=>{
    closed=true;if(wait)reject(new Error(`Worker exited ${code}: ${stderr}`));resolve(code);
  }));
  child.stdout.setEncoding('utf8');
  child.stdout.on('data',chunk=>{
    buffer+=chunk;if(buffer.length>262144){reject(new Error('Worker output exceeds limit'));child.kill();return;}
    let n;while((n=buffer.indexOf('\n'))>=0){
      const line=buffer.slice(0,n);buffer=buffer.slice(n+1);
      try{
        const result=JSON.parse(line);if(!wait)throw new Error('Unsolicited worker output');
        if(wait.id!=null && result.id!==wait.id)throw new Error('Worker request ID mismatch');
        const current=wait;wait=null;clearTimeout(current.timer);
        if(result.error)current.reject(new Error(result.error));else current.resolve(result);
      }catch(e){reject(e);}
    }
  });
  const receive=(id)=>new Promise((resolve,rejectPromise)=>{
    if(wait||closed||failure){rejectPromise(failure??new Error('Worker unavailable/busy'));return;}
    const timer=setTimeout(()=>{reject(new Error(`Worker timeout: ${stderr}`));child.kill();},timeoutMs);
    wait={id,resolve,reject:rejectPromise,timer};
  });
  let ready;
  try{ready=await receive(null);if(ready.type!=='ready')throw new Error('Missing worker readiness');}
  catch(e){child.kill();await exitPromise;throw e;}
  return {ready:{...ready,processStartupMs:performance.now()-launched},
    async request(id,pcm){
      const response=receive(id);
      const line=JSON.stringify({id,sampleRate:48000,channels:1,pcm:Buffer.from(pcm.buffer,pcm.byteOffset,pcm.byteLength).toString('base64')})+'\n';
      child.stdin.write(line,e=>{if(e)reject(e);});return response;
    },
    async close(){
      child.stdin.end();const timer=setTimeout(()=>child.kill(),5000);
      const exitCode=await exitPromise;clearTimeout(timer);
      if(exitCode!==0)throw new Error(`Worker failed to close normally: ${exitCode}; ${stderr}`);
      return stderr;
    }
  };
}
