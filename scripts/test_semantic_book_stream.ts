// Targeted native-worker protocol smoke: run with the authenticated Bun and installation directory.
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import {Worker} from 'node:worker_threads';
import {spawnSync} from 'node:child_process';
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
async function childContract(start:any,following:any,error:string){
  const worker=new Worker(new URL(path.join(installation,'semantic_pristine_worker.ts'),'file:'),{smol:true} as Bun.WorkerOptions);
  const exited=once(worker,'exit');
  const timer=setTimeout(()=>{void worker.terminate();throw Error('pristine-contract-timeout');},5000);
  try {
    const first=once(worker,'message');worker.postMessage(start);const [loaded]=await first;
    if(following){
      assert.equal(loaded.kind,'loaded');const reply=once(worker,'message');worker.postMessage(following);const [fatal]=await reply;
      assert.equal(fatal.kind,'fatal');assert.match(fatal.error,new RegExp(error));
    }else {assert.equal(loaded.kind,'fatal');assert.match(loaded.error,new RegExp(error));}
    const [code]=await exited;assert.equal(code,0);
  }finally {clearTimeout(timer);await worker.terminate();}
}
try {
  const root=path.join(fixture,'main.bend');
  await nativeBookControls(root);
  fs.writeFileSync(root,'import Base\n\ndef main() -> U32:\n  1\n');
  await childContract({kind:'start',request:1,root},{kind:'next',request:1,sequence:1},'credit-protocol');
  await childContract({kind:'start',request:1,root},{kind:'finish',request:1},'premature-finish');
  await childContract({kind:'start',request:1,root},{kind:'start',request:2,root},'start-protocol');
  await childContract({kind:'start',request:2,root},null,'start-protocol');
  fs.writeFileSync(root,'import forbidden@0.0.0.0/main.bend as remote\n');
  await childContract({kind:'start',request:1,root},null,'network-forbidden');
  const empty=new Worker(new URL(path.join(installation,'semantic_pristine_worker.ts'),'file:'),{smol:true} as Bun.WorkerOptions);
  await empty.terminate();
  const request={schema:1,id:'failure-contract',root:'main.bend',files:{'main.bend':'import Base\n\ndef main() -> U32:\n  1\n'},revisions:{'main.bend':1}};
  for(const [name,source,error]of [
    ['crash',"throw Error('deliberate-worker-crash');",'deliberate-worker-crash'],
    ['eof','process.exit(0);','pristine-unexpected-eof'],
    ['malformed',"import {parentPort} from 'node:worker_threads';parentPort.on('message',()=>parentPort.postMessage({kind:'wrong'}));",'pristine-unsolicited-or-stale-reply'],
  ]){
    const copy=path.join(fixture,name);fs.cpSync(installation,copy,{recursive:true});
    fs.writeFileSync(path.join(copy,'semantic_pristine_worker.ts'),source);
    const result=spawnSync(process.execPath,['--smol',path.join(copy,'worker.ts')],{input:JSON.stringify(request)+'\n',encoding:'utf8',timeout:5000,maxBuffer:4000000});
    assert.equal(result.error,undefined);assert.equal(result.status,0,result.stderr);
    const answer=JSON.parse(result.stdout);assert.equal(answer.outcome,'oracle-error');assert.ok(answer.error.includes(error),answer.error);
  }
  const eof=spawnSync(process.execPath,['--smol',path.join(installation,'worker.ts')],{input:'',encoding:'utf8',timeout:5000,maxBuffer:4000000});
  assert.equal(eof.error,undefined);assert.equal(eof.status,0,eof.stderr);assert.equal(eof.stdout,'');
  console.log('Exact bounded stream, mutation/reorder/truncation, credit/id/finish/overlap, forbidden network and termination contracts passed');
}finally {fs.rmSync(fixture,{recursive:true,force:true});}
