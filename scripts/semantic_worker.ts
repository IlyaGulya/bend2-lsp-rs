// Test-only persistent frontend observer. Never calls a compiler evaluator/backend/main.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import * as readline from 'node:readline';
import * as C from './instrumented.ts';
import {Worker as ThreadWorker} from 'node:worker_threads';
import {edenGC,gcAndSweep} from 'bun:jsc';
import {bookChunks,compareBatch,validateBatch} from './semantic_book_stream.ts';

const IDLE_HEAP_BYTES=32*1024*1024;

let networkAttempted=false;
globalThis.fetch = async () => { networkAttempted=true;throw new Error('network-forbidden'); };
const provenance = JSON.parse(fs.readFileSync(new URL('./provenance.json', import.meta.url), 'utf8'));
const hash = (text: string) => new Bun.CryptoHasher('sha256').update(text).digest('hex');
type Site = {file:string,start:number,end:number};
const siteKey = (s:Site) => `${s.file}:${s.start}:${s.end}`;
const stable = (x:any):string => JSON.stringify(x, (_key,value) => value && typeof value === 'object' && !Array.isArray(value) ? Object.fromEntries(Object.entries(value).sort(([a],[b])=>a.localeCompare(b))) : value);
function checkPath(name:string) {
  if (!/^[A-Za-z_][A-Za-z0-9_/-]*\.bend$/.test(name) || name.split('/').some(x=>x==='..'||x==='')) throw Error('unsafe-path');
}
function starts(s:string) { const out=[0]; for(let i=0;i<s.length;i++) if(s[i]==='\n')out.push(i+1); return out; }
function sourceCoordinates(text:string,parsed:string) {
  const original=starts(text),byteStarts=new Uint32Array(original.length),asciiLines=new Uint8Array(original.length);
  let bytes=0;
  for(let line=0;line<original.length;line++){
    byteStarts[line]=bytes;
    const segment=text.slice(original[line],original[line+1]??text.length),length=Buffer.byteLength(segment);
    asciiLines[line]=Number(length===segment.length);bytes+=length;
  }
  return {text,original,parsed:starts(parsed),byteStarts,asciiLines};
}
class Pristine {
  private thread=new ThreadWorker(new URL('./semantic_pristine_worker.ts',import.meta.url));
  private waiting:any=null;
  private queued:any=null;
  private serial=0;
  private expected:string|null=null;
  failed:Error|null=null;
  active=false;
  private stopping=false;
  constructor(){
    this.thread.on('message',(message:any)=>{
      if(message?.kind==='fatal'){this.fail(Error('pristine-worker-failure:'+message.error));return;}
      if(!this.active||message?.kind!==this.expected||message.request!==this.serial){this.fail(Error('pristine-unsolicited-or-stale-reply'));return;}
      this.expected=null;
      if(this.waiting){const waiting=this.waiting;this.waiting=null;waiting.resolve(message);}
      else if(this.queued!==null)this.fail(Error('pristine-unsolicited-reply'));
      else this.queued=message;
    });
    this.thread.on('error',(error:any)=>this.fail(error));
    this.thread.on('exit',(code:number)=>{if(!this.stopping)this.fail(Error('pristine-unexpected-eof:'+code));});
  }
  private fail(error:Error){
    if(this.failed)return;
    this.failed=error;
    if(this.waiting){this.waiting.reject(error);this.waiting=null;}
    void this.thread.terminate();
  }
  private receive():Promise<any>{
    if(this.failed)return Promise.reject(this.failed);
    if(this.queued!==null){const message=this.queued;this.queued=null;return Promise.resolve(message);}
    if(this.waiting)return Promise.reject(Error('pristine-overlapping-receive'));
    const waiting=Promise.withResolvers<any>();this.waiting=waiting;return waiting.promise;
  }
  private send(message:any,expected:string){
    if(this.failed)throw this.failed;
    if(this.expected!==null)throw Error('pristine-overlapping-credit');
    this.expected=expected;this.thread.postMessage(message);
  }
  start(root:string){
    if(this.failed)throw this.failed;
    if(this.active||this.waiting||this.queued!==null)throw Error('pristine-overlapping-project');
    this.active=true;this.send({kind:'start',request:++this.serial,root},'loaded');
  }
  async loaded(){
    const message=await this.receive();
    if(message?.kind!=='loaded'||message.request!==this.serial||(message.error!==null&&typeof message.error!=='string')||!Number.isSafeInteger(message.holes)||message.holes<0||!Number.isSafeInteger(message.count)||message.count<0||!message.metrics)throw Error('pristine-load-protocol-failure');
    return message;
  }
  async rejected(){
    this.send({kind:'finish',request:this.serial},'finished');
    const message=await this.receive();
    if(message?.kind!=='finished'||message.request!==this.serial)throw Error('pristine-finish-protocol-failure');
    this.active=false;
  }
  async compare(book:any,metrics:Record<string,number>){
    const iterator=bookChunks(C,book),began=performance.now();let sequence=0,nativeSeconds=0;
    this.send({kind:'next',request:this.serial,sequence},'batch');
    while(true){
      const message=await this.receive();validateBatch(message,sequence,this.serial);
      if(message.count!==book.order.length||message.holes!==book.hols||!Number.isFinite(message.canonical_seconds)||message.canonical_seconds<0)throw Error('pristine-book-protocol-failure');
      // Credit the next bounded batch before computing these native keys.
      if(!message.eof)this.send({kind:'next',request:this.serial,sequence:sequence+1},'batch');
      const stage=performance.now();compareBatch(iterator,message);nativeSeconds+=(performance.now()-stage)/1000;
      if(message.eof){
        this.active=false;
        metrics.frontend_canonical_seconds=nativeSeconds;
        metrics.parity_canonical_seconds=message.canonical_seconds;
        metrics.parity_normalization_seconds=(performance.now()-began)/1000;
        return;
      }
      sequence++;
    }
  }
  async close(){
    this.stopping=true;
    this.fail(Error('pristine-worker-closed'));
    await this.thread.terminate();
  }
}
const pristine=new Pristine();
async function run(request:any) {
  const began=performance.now(),metrics:Record<string,number>={};
  networkAttempted=false;
  if(request.schema!==1 || typeof request.id!=='string' || typeof request.root!=='string' || !request.files || Object.keys(request.files).length<1 || Object.keys(request.files).length>4) throw Error('protocol-failure');
  checkPath(request.root);
  if(!(request.root in request.files))throw Error('missing-root');
  const inspected=request.inspect_base??[];
  if(!Array.isArray(inspected)||inspected.length>16||new Set(inspected).size!==inspected.length||inspected.some(name=>typeof name!=='string'||name.length>128))throw Error('invalid-base-inspection');
  let total=0;
  for(const [name,text]of Object.entries(request.files)){
    checkPath(name); if(typeof text!=='string')throw Error('protocol-failure');
    if(!Number.isInteger(request.revisions?.[name])||request.revisions[name]<-2147483648||request.revisions[name]>2147483647)throw Error('invalid-source-revision');
    const bytes=Buffer.byteLength(text);total+=bytes;if(bytes>16384||total>65536)throw Error('input-limit');
    for(const line of text.split('\n')) {
      const trimmed=line.trim();if(trimmed===''||trimmed.startsWith('#'))continue;
      if(!/^import(?:\s|$)/.test(trimmed))break;
      const m=/^import\s+(\S+)/.exec(trimmed);if(!m)break;if(m[1]==='Base')continue;
      const target=path.posix.normalize(path.posix.join(path.posix.dirname(name),m[1]));checkPath(target);
      if(!(target in request.files))throw Error('missing-file:'+target);
      if(m[1].includes('@')||m[1].startsWith('0x')||m[1].startsWith('/'))throw Error('external-input-forbidden');
    }
  }
  const sandbox=fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(),'bend-semantic-')));
  try {
    for(const [name,text]of Object.entries(request.files)){const at=path.join(sandbox,name);fs.mkdirSync(path.dirname(at),{recursive:true});fs.writeFileSync(at,text as string,{flag:'wx',mode:0o444});}
    const nativeBegan=performance.now();pristine.start(path.join(sandbox,request.root));
    metrics.fixture_io_seconds=(performance.now()-began)/1000;
    const files=new WeakMap<object,any>(), epochs=new WeakMap<object,number>();
    const decls=new Map<string,any>(), binds=new Map<string,Site>(), terms=new Map<string,any>();
    const retainUses=new Set<string>(),extraTerms:any[]=[];
    const typeOrigins:any[]=[];
    const revisions:Record<string,number>=Object.create(null);
    const currentOwners=new WeakMap<object,{key:string,identity:string,site:Site}>(),bodySites=new Map<string,Site>();
    const entityIdentity=(table:'tld'|'constructor',key:string)=>table+'|'+key;
    let owner=0;
    function origin(p:any,beg:number,end:number):Site {
      const f=files.get(p);if(!f)throw Error('origin-ambiguity');
      let lo=0,hi=f.parsed.length;while(lo+1<hi){const m=(lo+hi)>>>1;if(f.parsed[m]<=beg)lo=m;else hi=m;}
      const b=f.original[lo]+beg-f.parsed[lo];
      let line=lo;while(line+1<f.parsed.length&&f.parsed[line+1]<=end)line++;
      const e=f.original[line]+end-f.parsed[line];
      if(!Number.isInteger(b)||!Number.isInteger(e)||b<0||e<b||e>f.text.length)throw Error('origin-ambiguity');
      return {
        file:f.name,
        start:f.byteStarts[lo]+(f.asciiLines[lo]?b-f.original[lo]:Buffer.byteLength(f.text.slice(f.original[lo],b))),
        end:f.byteStarts[line]+(f.asciiLines[line]?e-f.original[line]:Buffer.byteLength(f.text.slice(f.original[line],e))),
      };
    }
    function token(p:any,s:any,k:string):Site|null {
      if(!s||!k)return null;
      const segment=p.str.slice(s.beg,s.end);const m=/^[+\-]?\s*([A-Za-z_][A-Za-z0-9_.]*)/.exec(segment);
      if(!m)return null;
      const spelling=m[1];
      const b=s.beg+m[0].length-spelling.length;return origin(p,b,b+spelling.length);
    }
    C.semantic_observer((event:any)=>{
      const p=event.p;
      if(event.event==='file'){
        if(!event.real||typeof event.original!=='string')throw Error('origin-ambiguity');
        const name=event.real===C.BASE_BEND?'@Base.bend':path.relative(sandbox,event.real).split(path.sep).join('/');
        if(name!=='@Base.bend'&&!(name in request.files))throw Error('sandbox-escape');
        if(name!=='@Base.bend')revisions[name]=request.revisions[name];
        files.set(p,{name,...sourceCoordinates(event.original,p.str)});return;
      }
      if(event.event==='owner'){epochs.set(p,++owner);currentOwners.delete(p);return;}
      if(event.event==='definition-end'){
        const definition=currentOwners.get(p);if(!definition||definition.key!==event.k)throw Error('definition-origin-ambiguity');
        if(event.has_body)bodySites.set(definition.identity,definition.site);return;
      }
      if(event.event==='declaration'){
        const site=origin(p,event.beg,event.end),identity=entityIdentity(event.kind==='constructor'?'constructor':'tld',event.k);
        decls.set(siteKey(site),{site,kind:event.kind,name:event.nm,key:event.k,identity});currentOwners.set(p,{key:event.k,identity,site});return;
      }
      // Base still parses/checks normally; its local occurrences are not output.
      // Preserve all declaration and implementation origins above this boundary.
      if(files.get(p)?.name==='@Base.bend')return;
      if(event.event==='type-origin'){typeOrigins.push(event.tm);return;}
      if(event.event==='retain-use'){
        const site=token(p,event.s,event.k);if(site)retainUses.add(siteKey(site));return;
      }
      if(event.event==='binding'){
        if(event.synthetic){binds.set(`${epochs.get(p)}:${event.i}`,origin(p,event.s.beg,event.s.end));return;}
        if(event.k==='_'){const anonymous=token(p,event.s,event.k);if(anonymous)terms.delete(siteKey(anonymous));return;}
        const site=token(p,event.s,event.k);if(site){
          const old=terms.get(siteKey(site));if(retainUses.has(siteKey(site))&&old&&old.role!=='binding')extraTerms.push(old);
          binds.set(`${epochs.get(p)}:${event.i}`,site);terms.set(siteKey(site),{site,role:'binding',target:site,binding:'local',ownerIdentity:currentOwners.get(p)?.identity});
        }return;
      }
      if(event.event==='call'){
        const tm=event.tm;if(tm.$!=='Var'&&tm.$!=='Ref')return;
        const site=token(p,tm.s,tm.k),record=site&&terms.get(siteKey(site));
        if(record&&record.role!=='binding')record.role=event.offload||tm.b===true||record.role==='offload'?'offload':'call';
        return;
      }
      if(event.event==='term'){
        const tm=event.tm;if(!['Var','Ref','ADT','Ctr'].includes(tm.$))return;
        const site=token(p,tm.s,tm.k);if(!site)return;
        if(terms.get(siteKey(site))?.role==='binding')return;
        const key=tm.$==='Var'&&tm.v?.$==='Ref'?tm.v.k:tm.k;
        const local=tm.$==='Var'&&tm.v===undefined?`${epochs.get(p)}:${tm.i}`:null;
        const previous=terms.get(siteKey(site));
        const role=tm.$==='ADT'?'type':tm.$==='Ctr'?'constructor':previous?.role==='call'||previous?.role==='offload'?previous.role:'value';
        terms.set(siteKey(site),{site,role,identity:entityIdentity(tm.$==='Ctr'?'constructor':'tld',key),local,ownerIdentity:currentOwners.get(p)?.identity});
      }
    });
    const book=C.book_nil();let instrumentedError:any=null;let stage=performance.now();
    try {
      await C.book_load(book,path.join(sandbox,request.root),'',new Map());
      metrics.frontend_load_seconds=(performance.now()-stage)/1000;C.semantic_observer(null);stage=performance.now();
      C.book_valid(book);metrics.frontend_check_seconds=(performance.now()-stage)/1000;
    } catch(error){instrumentedError=error;} finally {C.semantic_observer(null);}
    const plain=await pristine.loaded();Object.assign(metrics,plain.metrics);
    // Per-path phase durations overlap; only this and total_frontend_case_seconds are wall latency.
    metrics.native_paths_wall_seconds=(performance.now()-nativeBegan)/1000;
    if(networkAttempted)throw Error('network-forbidden');
    if(instrumentedError&&instrumentedError.$!=='Err')throw instrumentedError;
    if((plain.error!==null)!==Boolean(instrumentedError))throw Error('instrumentation-parity-failure');
    if(instrumentedError){
      if(C.err_show(instrumentedError)!==plain.error)throw Error('instrumentation-rejection-parity-failure');
      await pristine.rejected();metrics.total_frontend_case_seconds=(performance.now()-began)/1000;
      return {schema:1,id:request.id,root:request.root,outcome:'compiler-error',error:C.err_show(instrumentedError),declarations:[],occurrences:[],provenance,metrics};
    }
    if(book.hols!==plain.holes)throw Error('instrumentation-hole-parity-failure');
    await pristine.compare(book,metrics);
    if(book.hols>0){
      metrics.total_frontend_case_seconds=(performance.now()-began)/1000;
      return {schema:1,id:request.id,root:request.root,outcome:'compiler-error',rejection:'incomplete-input',error:'incomplete-compiler-input',hole_counts:{instrumented:book.hols,pristine:plain.holes},declarations:[],occurrences:[],provenance,metrics};
    }
    stage=performance.now();
    function markType(term:any,depth=0) {
      if(depth>512)throw Error('semantic-depth-limit');
      if(!term||typeof term!=='object')return;
      if(['Var','Ref','ADT','Ctr'].includes(term.$)&&term.s&&files.has(term.s.file)){
        const site=token(term.s.file,term.s,term.k);
        const record=site&&terms.get(siteKey(site));
        if(record&&record.role!=='binding'&&record.role!=='constructor')record.role='type';
      }
      for(const [key,value]of Object.entries(term))if(key!=='s'&&key!=='file') {
        if(Array.isArray(value))for(const item of value)markType(item,depth+1);
        else if(value&&typeof value==='object')markType(value,depth+1);
      }
    }
    const projectEntities=new Set([...decls.values()].filter(declaration=>declaration.site.file!=='@Base.bend').map(declaration=>declaration.identity));
    for(const key of book.order){
      if(!projectEntities.has(entityIdentity('tld',key)))continue;
      const declaration=book.tlds[key];markType(C.term_lower(declaration.T));
      if(declaration.$==='ADT')for(const ctr of declaration.c)markType(C.term_lower(ctr.T));
    }
    function markAnnotations(term:any,depth=0) {
      if(depth>512)throw Error('semantic-depth-limit');
      if(!term||typeof term!=='object')return;
      if(term.$==='Ann')markType(term.T);
      if(term.$==='Rwt')markType(term.p);
      for(const [key,value]of Object.entries(term))if(key!=='s'&&key!=='file'){
        if(Array.isArray(value))for(const item of value)markAnnotations(item,depth+1);
        else if(value&&typeof value==='object')markAnnotations(value,depth+1);
      }
    }
    for(const key of book.order){
      if(!projectEntities.has(entityIdentity('tld',key)))continue;
      const declaration=book.tlds[key];if(declaration.$==='Def'&&declaration.v)markAnnotations(C.term_lower(declaration.v));
    }
    for(const typeOrigin of typeOrigins)markType(typeOrigin);
    const entities=new Map<string,Site>();for(const d of decls.values())if(!entities.has(d.identity))entities.set(d.identity,d.site);
    const declarations=[...decls.values()].filter(d=>d.site.file!=='@Base.bend').map(d=>({site:d.site,kind:d.kind,target:entities.get(d.identity),name:d.name,signatureOwner:entities.get(d.identity),bodyOwner:bodySites.get(d.identity)??null}));
    const base_entities=inspected.map((name:string)=>{
      const identity=entityIdentity('tld',name),site=entities.get(identity);
      const declaration=[...decls.values()].find(d=>d.identity===identity&&d.site.file==='@Base.bend');
      if(!site||!declaration||site.file!=='@Base.bend')throw Error('missing-base-inspection:'+name);
      return {site,name,kind:declaration.kind,target:site,signatureOwner:site,bodyOwner:bodySites.get(identity)??null};
    });
    for(const declaration of declarations)terms.set(siteKey(declaration.site),{site:declaration.site,role:'binding',target:declaration.target,binding:'global'});
    const occurrences=[...terms.values(),...extraTerms].filter(t=>t.site.file!=='@Base.bend').map(t=>{
      if(t.role==='binding')return {site:t.site,role:t.role,target:t.target??null,binding:t.binding,status:t.target?'resolved':'unresolved',enclosing:t.binding==='global'?null:entities.get(t.ownerIdentity)??null};
      const target=t.local?binds.get(t.local):entities.get(t.identity);
      return {site:t.site,role:t.role,target:target??null,binding:t.local?'local':'global',status:target?'resolved':'unresolved',enclosing:entities.get(t.ownerIdentity)??null};
    });
    const sort=(a:any,b:any)=>siteKey(a.site).localeCompare(siteKey(b.site))||stable(a).localeCompare(stable(b));
    const response={schema:1,id:request.id,root:request.root,outcome:'valid',declarations:declarations.sort(sort),occurrences:occurrences.sort(sort),base_entities:base_entities.sort(sort),complete:occurrences.every(occurrence=>Boolean(occurrence.target)),revisions,provenance,input_sha256:hash(stable(request.files)),metrics};
    metrics.normalization_seconds=(performance.now()-stage)/1000;metrics.total_frontend_case_seconds=(performance.now()-began)/1000;
    return response;
  } finally {if(pristine.active)await pristine.close();fs.rmSync(sandbox,{recursive:true,force:true});}
}
const input=readline.createInterface({input:process.stdin,crlfDelay:Infinity});
try {
  for await(const line of input){
    let request:any;
    try{if(Buffer.byteLength(line)>300000)throw Error('input-limit');request=JSON.parse(line);const response=await run(request);const output=JSON.stringify(response);if(Buffer.byteLength(output)>4000000)throw Error('output-limit');console.log(output);}
    catch(error){console.log(JSON.stringify({schema:1,id:request?.id??null,root:request?.root??null,outcome:'oracle-error',error:String(error),provenance}));if(pristine.failed)break;}
    finally {if(edenGC()>IDLE_HEAP_BYTES)gcAndSweep();}
  }
} finally {input.close();await pristine.close();}
