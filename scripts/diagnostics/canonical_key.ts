// Differential correctness against the authenticated compiler, not a second encoder.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {edenGC,gcAndSweep} from 'bun:jsc';
import type * as PinnedCompiler from '../../tests/fixtures/semantic/compiler/bend.ts';
import {canonicalTermKey} from '../semantic_book_stream.ts';

type NativeCompiler=Pick<typeof PinnedCompiler,'book_nil'|'book_load'|'book_valid'|'term_lower'|'term_key'|'err_show'>;
type Book=PinnedCompiler.Book;
type HTerm=PinnedCompiler.HTerm;
type Err=PinnedCompiler.Err;
type Facet='valid'|'compiler-error'|'recovery';
interface CorpusCase {
  id:string;input_sha256:string;facet:Facet;expected_error?:string;expected_rejection?:string;
  request:{root:string;files:Record<string,string>};
}
interface ProjectResult<T> {value:T|undefined;outcome:'valid'|'compiler-error';rejection:'compiler-error'|'incomplete-input'|null;error:string|null;holes:number;}
interface KeyResult {keys:number;canonical_sha256:string;}
interface Observation {id:string;input_sha256:string;facet:Facet;outcome:'valid'|'compiler-error';rejection:string|null;error:string|null;holes:number;keys:number;canonical_sha256?:string;}
interface CostSample {variant:'native'|'candidate';milliseconds:number;}
interface CostPair {pair:number;samples:CostSample[];}
interface CorpusReport {
  schema:1;status:'incomplete'|'pass';source:string|null;runtime:{version:string;revision:string};
  modules:Array<{module:string;completed:Observation[]}>;performance_admission:false;
}
function cases(text:string):CorpusCase[] {
  const decoded:unknown=JSON.parse(text);assert.ok(Array.isArray(decoded),'expected generated corpus cases');
  const values:readonly unknown[]=decoded;
  return values.map(value=>{
    assert.ok(value!==null&&typeof value==='object'&&'id' in value&&'input_sha256' in value&&'facet' in value&&'request' in value,'invalid generated case');
    const {id,input_sha256,facet}=value;
    const expected_error='expected_error' in value?value.expected_error:undefined;
    const expected_rejection='expected_rejection' in value?value.expected_rejection:undefined;
    assert.ok(typeof id==='string'&&typeof input_sha256==='string');
    assert.ok(facet==='valid'||facet==='compiler-error'||facet==='recovery');
    assert.ok(expected_error===undefined||typeof expected_error==='string');
    assert.ok(expected_rejection===undefined||typeof expected_rejection==='string');
    const request=value.request;
    assert.ok(request!==null&&typeof request==='object'&&'root' in request&&'files' in request);
    const root=request.root;assert.ok(typeof root==='string');
    assert.ok(request.files!==null&&typeof request.files==='object'&&!Array.isArray(request.files));
    const files:Record<string,string>=Object.create(null);
    const entries:Array<[string,unknown]>=Object.entries(request.files);
    for(const [name,source]of entries){
      assert.ok(typeof source==='string');assert.ok(!path.isAbsolute(name)&&!name.split('/').includes('..'),'case path escapes fixture');
      files[name]=source;
    }
    assert.ok(Object.hasOwn(files,root),'case root missing from files');
    return {id,input_sha256,facet,expected_error,expected_rejection,request:{root,files}};
  });
}
function compiler(value:unknown):NativeCompiler {
  assert.ok(value!==null&&typeof value==='object','invalid authenticated compiler module');
  for(const name of ['book_nil','book_load','book_valid','term_lower','term_key','err_show']){
    const exported:unknown=Reflect.get(value,name);
    assert.equal(typeof exported,'function','missing compiler export '+name);
  }
  // The installer authenticated these bytes; signatures come from the same pinned source.
  const native=value as unknown as NativeCompiler;
  return native;
}
const [installation,requestsFile,output,mode]=process.argv.slice(2);
assert.ok(installation&&requestsFile&&output);
assert.ok(mode==='correctness'||mode==='hosted');
if(mode==='hosted')assert.ok(process.platform==='linux'&&process.env.GITHUB_ACTIONS==='true','performance measurements are hosted-only');
const requests=cases(fs.readFileSync(requestsFile,'utf8'));
const IDLE_HEAP_BYTES=32*1024*1024;
let networkAttempted=false;
globalThis.fetch=async()=>{networkAttempted=true;throw Error('network-forbidden');};
// Both compiler modules see identical physical paths for each generated case.
const fixtures=fs.mkdtempSync(path.join(os.tmpdir(),'bend-canonical-'));

function* terms(book:Book):Generator<[string,HTerm]> {
  for(const key of book.order){
    const declaration=book.tlds[key];
    yield [key+'.type',declaration.T];
    if(declaration.$==='Def'){
      if(declaration.v!==null)yield [key+'.body',declaration.v];
    }else for(const constructor of declaration.c)yield [key+'.'+constructor.k,constructor.T];
  }
}
async function project<T extends object>(native:NativeCompiler,item:CorpusCase,inspect:(book:Book)=>T):Promise<ProjectResult<T>> {
  const directory=path.join(fixtures,createHash('sha256').update(item.id).digest('hex'));
  fs.mkdirSync(directory);
  try {
    for(const [name,text]of Object.entries(item.request.files)){
      const file=path.join(directory,name);fs.mkdirSync(path.dirname(file),{recursive:true});fs.writeFileSync(file,text,{mode:0o444});
    }
    const book=native.book_nil();let rejected:Err|undefined;networkAttempted=false;
    try{await native.book_load(book,path.join(directory,item.request.root),'',new Map());native.book_valid(book);}
    catch(error){
      if(error===null||typeof error!=='object'||!('$' in error)||error.$!=='Err')throw error;
      // An Err discriminant thrown by the authenticated compiler identifies its native error union.
      const nativeError=error as unknown as Err;
      rejected=nativeError;
    }
    assert.equal(networkAttempted,false,item.id+': unexpected network access');
    if(rejected){
      assert.notEqual(item.facet,'valid',item.id+': valid input rejected');
      const message=native.err_show(rejected);
      if(item.expected_error)assert.ok(message.includes(item.expected_error),item.id+': wrong compiler rejection');
      assert.equal(item.expected_rejection,undefined,item.id+': wrong rejection kind');
      return {value:undefined,outcome:'compiler-error',rejection:'compiler-error',error:message,holes:book.hols};
    }
    const value=inspect(book);
    if(book.hols>0){assert.notEqual(item.facet,'valid',item.id+': unexpected incomplete input');assert.equal(item.expected_rejection,'incomplete-input',item.id+': wrong incomplete-input rejection');}
    else assert.equal(item.facet,'valid',item.id+': intended-invalid input accepted');
    return {value,outcome:book.hols>0?'compiler-error':'valid',rejection:book.hols>0?'incomplete-input':null,error:book.hols>0?'incomplete-compiler-input':null,holes:book.hols};
  }finally{fs.rmSync(directory,{recursive:true,force:true});}
}
function checkKeys(native:NativeCompiler,book:Book):KeyResult {
  const hash=createHash('sha256');let keys=0;
  for(const [label,term]of terms(book)){
    const lowered=native.term_lower(term),before=native.term_key(lowered);
    const source=Object.getOwnPropertyDescriptor(lowered,'s'),fields=Object.keys(lowered);
    assert.equal(canonicalTermKey(lowered),before,label+': canonical bytes differ');
    assert.equal(native.term_key(lowered),before,label+': input semantics mutated');
    assert.deepEqual(Object.keys(lowered),fields,label+': input fields mutated');
    assert.deepEqual(Object.getOwnPropertyDescriptor(lowered,'s'),source,label+': source annotation mutated');
    hash.update(JSON.stringify([label,before]));keys++;
  }
  return {keys,canonical_sha256:hash.digest('hex')};
}
let sink=0;
function benchmark(native:NativeCompiler,book:Book) {
  const nativeTerms=[...terms(book)].map(([,term])=>term);
  const pass=(candidate:boolean)=>{
    for(const term of nativeTerms){const lowered=native.term_lower(term);sink^=(candidate?canonicalTermKey(lowered):native.term_key(lowered)).length;}
  };
  // Both arms include the official lowering and the existing idle-heap policy.
  const measure=(candidate:boolean)=>{const began=performance.now();pass(candidate);if(edenGC()>IDLE_HEAP_BYTES)gcAndSweep();return performance.now()-began;};
  measure(false);measure(true);
  const pairs:CostPair[]=[];
  for(let pair=0;pair<7;pair++){
    const order=pair%2===0?[false,true]:[true,false];
    const samples=order.map((candidate):CostSample=>({variant:candidate?'candidate':'native',milliseconds:measure(candidate)}));
    pairs.push({pair:pair+1,samples});
  }
  return {pairs,terms_per_arm:nativeTerms.length,sink,scope:'Whole-Book official term_lower + canonical key + existing idle GC per arm; report-only micro-cost, not cold loading, full runtime p95, RSS or admission.'};
}
const report:CorpusReport={schema:1,status:'incomplete',source:process.env.GITHUB_SHA??null,runtime:{version:Bun.version,revision:Bun.revision},modules:[],performance_admission:false};
fs.mkdirSync(output,{recursive:true});
try{
  for(const moduleName of ['bend.ts','instrumented.ts']){
    // CLI selects the authenticated installation; a static value import would bypass that boundary.
    const native=compiler(await import(new URL(path.join(installation,moduleName),'file:').href));
    if(mode==='hosted'&&moduleName==='bend.ts'){
      const first=requests.find(item=>item.facet==='valid');assert.ok(first);
      const cost=await project(native,first,book=>benchmark(native,book));
      fs.writeFileSync(path.join(output,'paired-cost.json'),JSON.stringify({...cost.value,source:report.source,runtime:report.runtime,performance_admission:false},null,2)+'\n');
    }
    const entry:{module:string;completed:Observation[]}={module:moduleName,completed:[]};report.modules.push(entry);
    for(const item of requests){
      if(entry.completed.length>0&&edenGC()>IDLE_HEAP_BYTES)gcAndSweep();
      const checked=await project(native,item,book=>checkKeys(native,book));
      entry.completed.push({id:item.id,input_sha256:item.input_sha256,facet:item.facet,outcome:checked.outcome,rejection:checked.rejection,error:checked.error,holes:checked.holes,...checked.value??{keys:0}});
    }
    fs.writeFileSync(path.join(output,'corpus-equivalence.json'),JSON.stringify(report,null,2)+'\n');
  }
  report.status='pass';
  console.log(JSON.stringify({status:'pass',modules:report.modules.map(entry=>({module:entry.module,views:entry.completed.length,keys:entry.completed.reduce((sum,row)=>sum+row.keys,0)}))}));
}finally{
  fs.writeFileSync(path.join(output,'corpus-equivalence.json'),JSON.stringify(report,null,2)+'\n');
  fs.rmSync(fixtures,{recursive:true,force:true});
}
