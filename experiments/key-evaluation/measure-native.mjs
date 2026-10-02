// Serial A/B harness around the existing, non-diagnostic synthetic evaluator.
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {stats} from './contract.mjs';
const [defaultExe,o2Exe,directory]=process.argv.slice(2);
if(!defaultExe||!o2Exe||!directory||process.argv.length!==5)throw new Error('Usage: measure-native.mjs default-key_benchmark.exe o2-key_benchmark.exe new-output-directory');
if(fs.existsSync(directory))throw new Error('Choose a fresh output directory');
fs.mkdirSync(directory,{recursive:true});
const bins={default:path.resolve(defaultExe),o2:path.resolve(o2Exe)},runs={default:[],o2:[]};
const hashes=Object.fromEntries(Object.entries(bins).map(([name,p])=>[name,createHash('sha256').update(fs.readFileSync(p)).digest('hex')]));
for(let repeat=0;repeat<3;repeat++){
  for(const name of repeat%2?['o2','default']:['default','o2']){
    const report=JSON.parse(execFileSync(bins[name],['--synthetic'],{windowsHide:true,maxBuffer:4*1024*1024,timeout:240000}).toString());
    fs.writeFileSync(path.join(directory,`${name}-${repeat}.json`),JSON.stringify(report,null,2)+'\n',{flag:'wx'});
    runs[name].push(report);process.stderr.write(`${name} repeat ${repeat+1}/3 complete\n`);
  }
}
const summarize=items=>Object.fromEntries(['batch','incremental'].map(mode=>{
  const cases=items.flatMap(r=>r.cases),timings=cases.flatMap(c=>c[mode].windows.slice(1).map(w=>w.computeMs));
  if(timings.some(v=>!Number.isFinite(v)))throw new Error('Unexpected evaluator timing schema');
  return [mode,{warm:stats(timings),cold:stats(cases.map(c=>c[mode].coldComputeMs)),finalCorrect:cases.every(c=>c[mode].finalCorrect),firstCorrectSignalSeconds:[...new Set(cases.map(c=>c[mode].firstCorrectAudioSeconds))]}];
}));
const snapshot=report=>report.cases.map(c=>({id:c.id,batch:c.batch.windows.map(w=>[w.audioEndSeconds,w.candidate,w.confirmed]),incremental:c.incremental.windows.map(w=>[w.audioEndSeconds,w.candidate,w.confirmed])}));
const reference=JSON.stringify(snapshot(runs.default[0]));
const outcomesIdentical=Object.values(runs).flat().every(r=>JSON.stringify(snapshot(r))===reference);
const result={hashes,repeats:3,executionOrder:'default,o2 / o2,default / default,o2',
  diagnosticsEnabled:false,rustProfile:'debug for both',outcomesIdentical,
  default:summarize(runs.default),o2:summarize(runs.o2)};
fs.writeFileSync(path.join(directory,'summary.json'),JSON.stringify(result,null,2)+'\n',{flag:'wx'});
console.log(JSON.stringify(result,null,2));
