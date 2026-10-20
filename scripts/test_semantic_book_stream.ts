// Targeted native-worker protocol smoke: run with the authenticated Bun and installation directory.
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import {spawn,spawnSync} from 'node:child_process';
import {Worker as ThreadWorker} from 'node:worker_threads';
import * as readline from 'node:readline';
import {once} from 'node:events';
import {bookChunks,compareBatch,validateBatch,BATCH_CHUNKS,CHUNK_CHARS} from './semantic_book_stream.ts';
const installation=process.argv[2];
assert.ok(installation,'supply the fresh native installation directory');
const fixture=fs.mkdtempSync(path.join(os.tmpdir(),'bend-pristine-contract-'));
const native={term_lower:(term:string)=>term,term_key:(term:string)=>term};
// Protocol specimens only, never a replacement semantic oracle.
const book={hols:0,order:['A','D'],tlds:{A:{$:'ADT',T:'Type',c:[{k:'Ctor',T:'x'.repeat(CHUNK_CHARS+1)}]},D:{$:'Def',T:'Type',v:'body'}}};
const chunks=[...bookChunks(native,book)];
const batch={kind:'batch',request:1,sequence:0,chunks,eof:true};
validateBatch(batch,0,1);compareBatch(bookChunks(native,book),batch);
for(const mutation of [
  {...batch,request:2},{...batch,sequence:1},{...batch,eof:false,chunks:[]},
  {...batch,chunks:Array(BATCH_CHUNKS+1).fill(chunks[0])},
  {...batch,chunks:[[0,0,'x'.repeat(CHUNK_CHARS+1),true]]},
  {...batch,chunks:Array(5).fill([0,0,'x'.repeat(CHUNK_CHARS),true])},
  {...batch,chunks:[[0,-1,'x',true]]},
])assert.throws(()=>validateBatch(mutation,0,1));
for(const mutation of [
  {...batch,chunks:chunks.slice(0,-1)},
  {...batch,chunks:[chunks[1],chunks[0],...chunks.slice(2)]},
  {...batch,chunks:[...chunks,chunks[0]]},
  {...batch,chunks:chunks.map((chunk,index)=>index===2?[chunk[0],chunk[1],chunk[2]+'!',chunk[3]]:chunk)},
])assert.throws(()=>compareBatch(bookChunks(native,book),mutation));
async function nativeBookControls(root:string) {
  // The CLI selects an authenticated captured installation; a static import would bypass it.
  const native=await import(new URL(path.join(installation,'bend.ts'),'file:').href);
  const header='type T is Data:\n  A{}\n  B{}\ntype U is Data:\n  C{}\ntype Holder is Data:\n  Wrap{value: ';
  async function checked(field:string,body:string) {
    fs.writeFileSync(root,header+field+'}\ndef main() -> T:\n  '+body+'\n');
    const result=native.book_nil();
    await native.book_load(result,root,'',new Map());native.book_valid(result);
    return result;
  }
  const original=await checked('T','A{}');
  const different=await checked('U','B{}');
  const definition=original.order.find((key:string)=>original.tlds[key].$==='Def');
  const holder=original.order.find((key:string)=>original.tlds[key].$==='ADT'&&original.tlds[key].c.some((ctor:any)=>ctor.n===1));
  const otherType=original.order.find((key:string)=>original.tlds[key].$==='ADT'&&original.tlds[key].c.some((ctor:any)=>ctor.k.endsWith('C')));
  assert.ok(definition&&holder&&otherType,'native control declarations must be present');
  const changed=(key:string,value:any)=>({...original,tlds:{...original.tlds,[key]:value}});
  const type=original.tlds[holder],constructor=type.c[0];
  const controls=[
    {...original,order:[original.order[1],original.order[0],...original.order.slice(2)]},
    {...original,hols:original.hols+1},
    changed(definition,{...original.tlds[definition],T:original.tlds[otherType].T}),
    changed(definition,{...original.tlds[definition],v:different.tlds[definition].v}),
    changed(holder,{...type,c:[]}),
    changed(holder,{...type,c:[{...constructor,k:constructor.k+'.changed'}]}),
    changed(holder,{...type,c:[{...constructor,T:different.tlds[holder].c[0].T}]}),
  ];
  for(const control of controls) {
    const message={kind:'batch',request:1,sequence:0,chunks:[...bookChunks(native,control)],eof:true};
    validateBatch(message,0,1);
    assert.throws(()=>compareBatch(bookChunks(native,original),message),/instrumentation-target-parity-failure/);
  }
}
async function childContract(start:any,following:any){
  const worker=new ThreadWorker(new URL(path.join(installation,'semantic_pristine_worker.ts'),'file:'));
  const closed=once(worker,'exit');
  const timer=setTimeout(()=>{void worker.terminate();throw Error('pristine-contract-timeout');},5000);
  try {
    const first=once(worker,'message');worker.postMessage(start);const [loaded]=await first;
    if(following){
      assert.equal(loaded.kind,'loaded');const reply=once(worker,'message');worker.postMessage(following);const [fatal]=await reply;
      assert.equal(fatal.kind,'fatal');
    }else {assert.equal(loaded.kind,'fatal');}
    const [code]=await closed;assert.equal(code,0);
  }finally {clearTimeout(timer);await worker.terminate();await closed;}
}
async function signalContract(signal:'SIGTERM'|'SIGINT',request:any){
  const copy=path.join(fixture,signal);fs.cpSync(installation,copy,{recursive:true});
  const grouped=process.platform!=='win32';
  const worker=spawn(process.execPath,[path.join(copy,'worker.ts')],{detached:grouped,stdio:['pipe','pipe','pipe']});
  const closed=once(worker,'close'),output=readline.createInterface({input:worker.stdout});
  let stderr='';worker.stderr.on('data',chunk=>{stderr+=chunk.toString();});
  const timeout=Promise.withResolvers<never>();
  const timer=setTimeout(()=>timeout.reject(Error('pristine-signal-timeout:'+signal)),5000);
  try {
    const reply=once(output,'line');worker.stdin.write(JSON.stringify(request)+'\n');
    const [line]=await Promise.race([reply,timeout.promise]);
    assert.equal(JSON.parse(line).outcome,'valid',line);
    assert.ok(worker.pid);
    if(grouped)process.kill(-worker.pid,signal);else worker.kill(signal);
    const [code,termination]=await Promise.race([closed,timeout.promise]);
    assert.equal(termination,null,stderr);assert.equal(code,signal==='SIGTERM'?143:130,stderr);
    if(grouped)assert.throws(()=>process.kill(-worker.pid!,0),(error:any)=>error.code==='ESRCH','owned process group must be absent');
  }finally {
    clearTimeout(timer);output.close();
    if(worker.exitCode===null&&worker.signalCode===null){
      if(grouped&&worker.pid)process.kill(-worker.pid,'SIGKILL');else worker.kill('SIGKILL');
    }
    await closed;
  }
}
try {
  const root=path.join(fixture,'main.bend');
  await nativeBookControls(root);
  fs.writeFileSync(root,'import Base\n\ndef main() -> U32:\n  1\n');
  await childContract({kind:'start',request:1,root},{kind:'next',request:1,sequence:1});
  await childContract({kind:'start',request:1,root},{kind:'finish',request:1});
  await childContract({kind:'start',request:1,root},{kind:'start',request:2,root});
  await childContract({kind:'start',request:2,root},null);
  fs.writeFileSync(root,'import forbidden@0.0.0.0/main.bend as remote\n');
  await childContract({kind:'start',request:1,root},null);
  const empty=new ThreadWorker(new URL(path.join(installation,'semantic_pristine_worker.ts'),'file:'));
  const emptyClosed=once(empty,'exit');await empty.terminate();await emptyClosed;
  const request={schema:1,id:'failure-contract',root:'main.bend',files:{'main.bend':'import Base\n\ndef main() -> U32:\n  1\n'},revisions:{'main.bend':1}};
  await signalContract('SIGTERM',request);
  await signalContract('SIGINT',request);
  for(const [name,source]of [
    ['crash',"throw Error('deliberate-worker-crash');"],
    ['eof','process.exit(0);'],
    ['malformed',"import {parentPort} from 'node:worker_threads';parentPort.on('message',()=>parentPort.postMessage({kind:'wrong'}));"],
    ['stdout',"import {parentPort} from 'node:worker_threads';console.log('deliberate-noise');parentPort.on('message',()=>{});"],
  ]){
    const copy=path.join(fixture,name);fs.cpSync(installation,copy,{recursive:true});
    fs.writeFileSync(path.join(copy,'semantic_pristine_worker.ts'),source);
    const result=spawnSync(process.execPath,[path.join(copy,'worker.ts')],{input:JSON.stringify(request)+'\n',encoding:'utf8',timeout:5000,maxBuffer:4000000});
    assert.equal(result.error,undefined);assert.equal(result.status,0,result.stderr);
    const answer=JSON.parse(result.stdout);assert.equal(answer.outcome,'oracle-error');
  }
  const eof=spawnSync(process.execPath,[path.join(installation,'worker.ts')],{input:'',encoding:'utf8',timeout:5000,maxBuffer:4000000});
  assert.equal(eof.error,undefined);assert.equal(eof.status,0,eof.stderr);assert.equal(eof.stdout,'');
  console.log('Native canonical mutation/reorder/truncation, credit/id/finish/overlap, forbidden network, stdout rejection and bounded signal termination contracts passed');
}finally {fs.rmSync(fixture,{recursive:true,force:true});}
