// Targeted native-worker protocol smoke: run with authenticated Bun, installation and Python executable.
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import {spawn,spawnSync} from 'node:child_process';
import {Worker as ThreadWorker} from 'node:worker_threads';
import * as readline from 'node:readline';
import {once} from 'node:events';
import {fileURLToPath} from 'node:url';
import {bookChunks,canonicalTermKey,compareBatch,validateBatch,BATCH_CHUNKS,CHUNK_CHARS} from './semantic_book_stream.ts';
import type {Book,File,HTerm,LTerm,Patt,Quant,Span} from '../tests/fixtures/semantic/compiler/bend.ts';
// Structural view of the pinned compiler exports used by these fixtures.
type Native={
  term_key(term:LTerm):string;
  term_lower(term:HTerm,d?:number):LTerm;
  book_nil():Book;
  book_load(book:Book,file:string,ns:string,seen:Map<string,string|null>):Promise<number>;
  book_valid(book:Book):void;
  None():Quant;
  Lone():Quant;
  Many():Quant;
  Var(k:string,i:number,s?:Span,v?:HTerm):LTerm;
  // A Ref leaf is shared by LTerm and HTerm; the pinned types differ only in binder bodies.
  Ref(k:string,s?:Span,b?:boolean):LTerm & HTerm;
  Sub(i:number,v:Patt,f:LTerm,s?:Span):LTerm;
  Let(k:string[],i:number[],v:LTerm[],f:LTerm,s?:Span,q?:Quant[]):LTerm;
  Typ(g:LTerm,s?:Span):LTerm;
  Qnt(s?:Span):LTerm;
  Qua(q:Quant,s?:Span):LTerm;
  Min(a:LTerm,b:LTerm,s?:Span):LTerm;
  All(q:Quant,k:string,i:number,A:LTerm,B:LTerm,s?:Span):LTerm;
  Lam(k:string,i:number,f:LTerm,s?:Span,q?:Quant):LTerm;
  App(f:LTerm,x:LTerm,s?:Span):LTerm;
  ADT(k:string,x:LTerm[],s?:Span,r?:string[]):LTerm;
  Ctr(k:string,x:LTerm[],s?:Span):LTerm;
  Lit(k:'Nat'|'U32'|'F32'|'String',v:number|string,s?:Span):LTerm;
  Mat(k:string,h:LTerm,m:LTerm,s?:Span):LTerm;
  Efq(s?:Span):LTerm;
  Eql(a:LTerm,b:LTerm,T:LTerm,s?:Span):LTerm;
  Rfl(s?:Span):LTerm;
  Rwt(e:LTerm,p:LTerm,f:LTerm,s?:Span):LTerm;
  Hol(k:string,s?:Span):LTerm;
  Ann(x:LTerm,T:LTerm,s?:Span):LTerm;
};
const installation=process.argv[2];
assert.ok(installation,'supply the fresh native installation directory');
const fixture=fs.mkdtempSync(path.join(os.tmpdir(),'bend-pristine-contract-'));
// Identity lowering isolates tuple/batch boundaries; semantic contracts below use the real compiler.
const protocolSpecimen={term_lower:(term:string)=>term};
const book={hols:0,order:['A','D'],tlds:{A:{$:'ADT',T:'Type',c:[{k:'Ctor',T:'x'.repeat(CHUNK_CHARS+1)}]},D:{$:'Def',T:'Type',v:'body'}}};
const chunks=[...bookChunks(protocolSpecimen,book)];
const batch={kind:'batch',request:1,sequence:0,chunks,eof:true};
validateBatch(batch,0,1);compareBatch(bookChunks(protocolSpecimen,book),batch);
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
])assert.throws(()=>compareBatch(bookChunks(protocolSpecimen,book),mutation));
// Every tuple field is part of parity, even when the batch remains well formed.
for(const [field,replacement] of [[0,chunks[2][0]+1],[1,chunks[2][1]+1],[2,chunks[2][2]+'!'],[3,!chunks[2][3]]] as const) {
  const changed=chunks.map(chunk=>[...chunk]);
  changed[2][field]=replacement;
  const mutation={...batch,chunks:changed};
  validateBatch(mutation,0,1);
  assert.throws(()=>compareBatch(bookChunks(protocolSpecimen,book),mutation),/instrumentation-target-parity-failure/);
}
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
// Canonical native term keys: every non-source field, ordering and scalar must match
// the pinned compiler's term_key byte for byte without touching the input or its spans.
// Closed Span shape: only source annotations are inspected here, never untrusted JSON.
function isSpan(value:unknown):value is Span {
  if(value===null||typeof value!=='object'||!('beg' in value)||!('end' in value)||!('file' in value))return false;
  const file:unknown=value.file;
  if(file===null||typeof file!=='object'||!('str' in file)||!('ns' in file)||!('al' in file))return false;
  if(typeof value.beg!=='number'||typeof value.end!=='number')return false;
  if(typeof file.str!=='string'||typeof file.ns!=='string')return false;
  const al:unknown=file.al;
  if(al===null||typeof al!=='object')return false;
  for(const key of Object.keys(al))if(typeof Reflect.get(al,key)!=='string')return false;
  return true;
}
function projection(value:unknown,seen=new Set<object>()):unknown {
  if(value===null||typeof value!=='object')return value;
  if(seen.has(value))return '<shared>';
  seen.add(value);
  if(Array.isArray(value)){
    const items:unknown[]=[];
    for(let index=0;index<value.length;index++){const item:unknown=value[index];items.push(projection(item,seen));}
    return items;
  }
  const out:{[key:string]:unknown}={};
  for(const key of Object.keys(value)){
    const child:unknown=Reflect.get(value,key);
    out[key]=key==='s'?'<source>':projection(child,seen);
  }
  return out;
}
type SpanSnapshot={span:Span;spanKeys:string[];beg:number;end:number;file:File;fileKeys:string[];fileStr:string;fileNs:string;fileAl:Record<string,string>};
function sourceSpans(value:unknown):SpanSnapshot[] {
  const spans:Span[]=[],seen=new Set<object>();
  const visit=(node:unknown):void=>{
    if(node===null||typeof node!=='object'||seen.has(node))return;
    seen.add(node);
    if(Array.isArray(node)){
      for(let index=0;index<node.length;index++){const item:unknown=node[index];visit(item);}
      return;
    }
    for(const key of Object.keys(node)){
      const child:unknown=Reflect.get(node,key);
      if(key==='s'){if(isSpan(child))spans.push(child);}
      else visit(child);
    }
  };
  visit(value);
  return spans.map(span=>({span,spanKeys:Object.keys(span),beg:span.beg,end:span.end,file:span.file,fileKeys:Object.keys(span.file),fileStr:span.file.str,fileNs:span.file.ns,fileAl:{...span.file.al}}));
}
function sameSourceSpans(actual:SpanSnapshot[],expected:SpanSnapshot[],label:string) {
  assert.equal(actual.length,expected.length,label+': source field count changed');
  for(let index=0;index<expected.length;index++){
    const actualSpan=actual[index],expectedSpan=expected[index];
    assert.equal(actualSpan.span,expectedSpan.span,label+': source span replaced');
    assert.deepEqual(actualSpan.spanKeys,expectedSpan.spanKeys,label+': source span fields changed');
    assert.equal(actualSpan.beg,expectedSpan.beg,label+': source span start changed');
    assert.equal(actualSpan.end,expectedSpan.end,label+': source span end changed');
    assert.equal(actualSpan.file,expectedSpan.file,label+': source file replaced');
    assert.deepEqual(actualSpan.fileKeys,expectedSpan.fileKeys,label+': source file fields changed');
    assert.equal(actualSpan.fileStr,expectedSpan.fileStr,label+': source text changed');
    assert.equal(actualSpan.fileNs,expectedSpan.fileNs,label+': source namespace changed');
    assert.deepEqual(actualSpan.fileAl,expectedSpan.fileAl,label+': source aliases changed');
  }
}
function canonicalContract(native:Native,term:LTerm,label:string) {
  const expected=native.term_key(term),shape=JSON.stringify(projection(term)),spans=sourceSpans(term);
  const actual=canonicalTermKey(term);
  assert.equal(actual,expected,label+': canonical key differs from native term_key');
  assert.equal(native.term_key(term),expected,label+': native key changed after canonicalization');
  assert.equal(JSON.stringify(projection(term)),shape,label+': input structure was mutated');
  sameSourceSpans(sourceSpans(term),spans,label);
}
type Extension={zulu:number;alpha:number;kept:(number|string|boolean|null)[];label:string};
type ExtendedTerm=LTerm & {semantic_extension?:Extension};
type NestedExtendedTerm=LTerm & {nested_extension?:{kept:(number|string|boolean|null)[]}};
interface DirectTerms {
  terms:Array<[string,LTerm]>;
  sharedTerm:LTerm;
  shared:[LTerm,LTerm];
  extended:ExtendedTerm;
  extraChild:NestedExtendedTerm;
  aliasPair:LTerm;
  sharedFile:File;
}
// Closed native term accessors; these fixtures construct only these shapes.
function constructorChildren(value:LTerm):LTerm[] {
  if(value.$!=='Ctr')throw Error('expected-native-constructor-term');
  return value.x;
}
function adtArguments(value:LTerm):LTerm[] {
  if(value.$!=='ADT')throw Error('expected-native-adt-term');
  return value.x;
}
function sourceSpan(value:LTerm):Span {
  if(value.s===undefined)throw Error('expected-native-source-span');
  return value.s;
}
function directCanonicalTerms(native:Native):DirectTerms {
  const span=(tag:string):Span=>({file:{str:'source-'+tag,ns:'contract',al:{Alias:'imported-'+tag}},beg:tag.length,end:tag.length+3});
  const strings=['','"','\\','\b','\f','\n','\r','\t','</script>&','\u007f','\u0080','\u00e9','\u4e2d','\ud83d\ude42','\u2028','\u2029','\ud800','\udfff'];
  const sharedLit=native.Lit('U32',4294967295,span('shared-leaf'));
  const sharedText=native.Lit('String',['shared','"','\\','\n','\ud800'].join(''),span('shared-text'));
  const sharedTerm=native.Ctr('Shared',[sharedLit,sharedText,sharedLit,sharedText],span('shared'));
  // A generic encoder must keep own enumerable non-source fields native term_key keeps.
  const extraChild:NestedExtendedTerm=native.Lit('String','extra',span('extra-child'));
  extraChild.nested_extension={kept:[1,'two',true,null]};
  const extended:ExtendedTerm=native.Ctr('Extras',[extraChild],span('extra'));
  extended.semantic_extension={zulu:1,alpha:2,kept:[1,'two',true,null],label:'own enumerable non-source field'};
  const sharedFile:File={str:'shared-source-file',ns:'contract',al:{Alias:'shared'}};
  const sharedSpan:Span={file:sharedFile,beg:5,end:6};
  const aliasPair=native.ADT('Alias',[native.Hol('alias:a',sharedSpan),native.Hol('alias:b',sharedSpan)],span('alias'));
  const terms:Array<[string,LTerm]>=[
    ['var',native.Var('x',0)],
    ['var-negative-index',native.Var('_',-1,span('var-cell'))],
    ['var-span',native.Var('x',1,span('var'))],
    ['var-substitution',native.Var('x',2,span('var'),native.Ref('y',span('var-substitution')))],
    ['ref',native.Ref('r')],
    ['ref-barrier',native.Ref('ns:r',span('ref'),true)],
    ['ref-no-barrier',native.Ref('ns:r',span('ref'),false)],
    ['sub-var',native.Sub(3,{$:'PVar',k:'p',i:0,q:native.Lone(),s:span('sub-pattern')},native.Var('p',0,span('sub-body')),span('sub'))],
    ['sub-constructor',native.Sub(4,{$:'PCtr',k:'C',x:[{$:'PVar',k:'q',i:1,q:native.Many(),s:span('sub-pattern-inner')},{$:'PCtr',k:'Nested',x:[{$:'PVar',k:'deep',i:2,q:native.None(),s:span('sub-pattern-deep')}],s:span('sub-pattern-nested')}],s:span('sub-pattern-ctor')},native.Ctr('C',[native.Var('q',1)],span('sub-ctor')),span('sub-constructor'))],
    ['let',native.Let(['a','b'],[5,6],[native.Lit('U32',0,span('let-value-0')),native.Var('a',5,span('let-value-1'))],native.Var('b',6,span('let-body')),span('let'),[native.Lone(),native.Many()])],
    ['let-default-quantifiers',native.Let(['a'],[7],[native.Ref('Unit',span('let-default-value'))],native.Ref('Empty'),span('let-default'))],
    ['typ',native.Typ(native.Typ(native.Qnt(span('qnt-inner')),span('typ-inner')),span('typ'))],
    ['qnt',native.Qnt(span('qnt'))],
    ['qua-none',native.Qua(native.None(),span('qua-none'))],
    ['qua-lone',native.Qua(native.Lone())],
    ['qua-many',native.Qua(native.Many(),span('qua-many'))],
    ['min',native.Min(native.Lit('U32',1,span('min-a')),native.Lit('U32',2,span('min-b')),span('min'))],
    ['all',native.All(native.Many(),'a',8,native.Typ(native.Qua(native.Lone(),span('all-domain-inner')),span('all-domain')),native.App(native.Var('a',8,span('all-body-var')),native.Ref('Unit'),span('all-body')),span('all'))],
    ['lam',native.Lam('x',9,native.Var('x',9,span('lam-body')),span('lam'),native.Many())],
    ['lam-default-quantifier',native.Lam('x',10,native.Ref('Unit'),span('lam-default'))],
    ['app',native.App(native.Ref('f',span('app-f')),native.Var('x',11,span('app-x')),span('app'))],
    ['adt',native.ADT('List',[native.Var('a',12),native.Var('b',13),native.Var('c',14)],span('adt'),['Hidden','Other'])],
    ['adt-default-fields',native.ADT('Unit',[])],
    ['ctr',native.Ctr('Con',[native.Lit('U32',0),native.Lit('String','x',span('ctr'))],span('ctr'))],
    ['ctr-empty',native.Ctr('Unit',[])],
    ['lit-nat-max',native.Lit('Nat',4294967295,span('lit-nat'))],
    ['lit-u32-zero',native.Lit('U32',0)],
    ['lit-u32-max',native.Lit('U32',4294967295,span('lit-u32'))],
    ['lit-f32-zero',native.Lit('F32',0)],
    ['lit-f32-negative-zero',native.Lit('F32',0x80000000)],
    ['lit-f32-max',native.Lit('F32',0x7f7fffff)],
    ['lit-f32-subnormal',native.Lit('F32',1)],
    ['lit-string-empty',native.Lit('String','')],
    ['lit-string-escapes',native.Lit('String',strings.join(''),span('lit-string'))],
    ['lit-string-controls',native.Lit('String',Array.from({length:32},(_,index)=>String.fromCharCode(index)).join(''))],
    ['mat',native.Mat('A',native.Ctr('A',[]),native.Ref('Empty',span('mat-fallback')),span('mat'))],
    ['efq',native.Efq(span('efq'))],
    ['eql',native.Eql(native.Lit('U32',1,span('eql-a')),native.Lit('U32',2,span('eql-b')),native.Ref('U32',span('eql-type')),span('eql'))],
    ['rfl',native.Rfl(span('rfl'))],
    ['rwt',native.Rwt(native.Ref('e',span('rwt-term')),native.Eql(native.Ref('a'),native.Ref('b'),native.Ref('A'),span('rwt-proof')),native.Lam('_',15,native.Ref('P',span('rwt-body')),span('rwt-lambda')),span('rwt'))],
    ['hol',native.Hol('ns:hole',span('hol'))],
    ['ann',native.Ann(native.Var('x',16,span('ann-term')),native.Ref('U32',span('ann-type')),span('ann'))],
    ['shared-leaves',sharedTerm],
    ['extra-fields',extended],
    ['aliased-source-file',aliasPair],
  ];
  return {terms,sharedTerm,shared:[sharedLit,sharedText],extended,extraChild,aliasPair,sharedFile};
}
function generatedNativeTerms(native:Native):LTerm[] {
  let state=0x51e5d1,budget=4096;
  const random=():number=>((state=(Math.imul(state,1664525)+1013904223)>>>0)/0x100000000);
  const pick=<Item>(values:readonly Item[]):Item=>{
    const item:Item|undefined=values[Math.floor(random()*values.length)];
    if(item===undefined)throw Error('empty-native-term-choice');
    return item;
  };
  const quant=():Quant=>pick([native.None(),native.Lone(),native.Many()]);
  const span=(tag:string):Span=>({file:{str:'generated-'+tag,ns:'generated',al:{Alias:'generated:'+tag}},beg:tag.length,end:tag.length+1});
  const pattern=(tag:string):Patt=>random()<0.5
    ?{$:'PVar',k:tag,i:Math.floor(random()*6),q:quant(),s:span(tag)}
    :{$:'PCtr',k:tag,x:[{$:'PVar',k:tag+'0',i:0,q:quant(),s:span(tag+'-inner')}],s:span(tag)};
  const leaf=(tag:string):LTerm=>pick<() => LTerm>([
    ()=>native.Var('v'+tag,Math.floor(random()*6)),
    ()=>native.Ref('r:'+tag,span(tag),random()<0.5?true:undefined),
    ()=>native.Lit('U32',Math.floor(random()*0x100000000)),
    ()=>native.Lit('Nat',Math.floor(random()*0x100000000)),
    ()=>native.Lit('F32',pick([0,1,0x7f7fffff,0x80000000,0x3f800000])),
    ()=>native.Lit('String',pick(['','a"b','\\','\n','\u00e9','\ud83d\ude42','\ud800'])),
    ()=>native.Qnt(span(tag)),
    ()=>native.Efq(),
    ()=>native.Rfl(span(tag)),
    ()=>native.Hol('h.'+tag,span(tag)),
    ()=>native.Qua(quant(),span(tag)),
  ])();
  const term=(depth:number,tag:string):LTerm=>{
    if(depth<=0||budget<=0)return leaf(tag);
    budget-=1;
    switch(Math.floor(random()*14)){
      case 0:return native.Typ(term(depth-1,tag+'g'),span(tag));
      case 1:return native.Min(term(depth-1,tag+'a'),term(depth-1,tag+'b'),span(tag));
      case 2:return native.All(quant(),'a'+tag,Math.floor(random()*6),term(depth-1,tag+'A'),term(depth-1,tag+'B'),span(tag));
      case 3:return native.Lam('l'+tag,Math.floor(random()*6),term(depth-1,tag+'f'),span(tag),random()<0.5?quant():undefined);
      case 4:return native.App(term(depth-1,tag+'f'),term(depth-1,tag+'x'),span(tag));
      case 5:return native.ADT('T'+tag,[term(depth-1,tag+'0'),term(depth-1,tag+'1')],span(tag),random()<0.5?[]:['R'+tag]);
      case 6:return native.Ctr('C'+tag,[term(depth-1,tag+'0')],span(tag));
      case 7:return native.Mat('M'+tag,term(depth-1,tag+'h'),term(depth-1,tag+'m'),span(tag));
      case 8:return native.Eql(term(depth-1,tag+'a'),term(depth-1,tag+'b'),term(depth-1,tag+'T'),span(tag));
      case 9:return native.Rwt(term(depth-1,tag+'e'),term(depth-1,tag+'p'),term(depth-1,tag+'f'),span(tag));
      case 10:return native.Ann(term(depth-1,tag+'x'),term(depth-1,tag+'T'),span(tag));
      case 11:return native.Sub(Math.floor(random()*6),pattern(tag),term(depth-1,tag+'f'),span(tag));
      case 12:return native.Let(['k'+tag+'0','k'+tag+'1'],[Math.floor(random()*6),Math.floor(random()*6)],[term(depth-1,tag+'v0'),term(depth-1,tag+'v1')],term(depth-1,tag+'f'),span(tag),[quant(),quant()]);
      default:return native.Var('v'+tag,Math.floor(random()*6),span(tag),native.Ref('v:'+tag,span(tag)));
    }
  };
  return Array.from({length:24},(_,index)=>term(4,'g'+index));
}
async function nativeCanonicalContracts(root:string) {
  // The CLI selects an authenticated captured installation; a static import would bypass it.
  const native:Native=await import(new URL(path.join(installation,'bend.ts'),'file:').href);
  const {terms,sharedTerm,shared,extended,extraChild,aliasPair,sharedFile}=directCanonicalTerms(native);
  for(const [label,term] of terms)canonicalContract(native,term,'direct '+label);
  const sharedChildren=constructorChildren(sharedTerm);
  for(const [index,leaf] of shared.entries()){
    assert.equal(sharedChildren[index],leaf,'shared native leaf must stay shared');
    assert.equal(sharedChildren[index+2],leaf,'shared native leaf must stay shared');
  }
  assert.deepEqual(extended.semantic_extension,{zulu:1,alpha:2,kept:[1,'two',true,null],label:'own enumerable non-source field'},'own non-source fields and order must survive canonicalization');
  assert.deepEqual(extraChild.nested_extension,{kept:[1,'two',true,null]},'nested own non-source fields must survive canonicalization');
  const aliasChildren=adtArguments(aliasPair);
  const aliasLeft=sourceSpan(aliasChildren[0]),aliasRight=sourceSpan(aliasChildren[1]);
  assert.equal(aliasLeft.file,sharedFile,'source file aliases must be preserved');
  assert.equal(aliasRight.file,sharedFile,'source file aliases must be preserved');
  assert.equal(aliasLeft,aliasRight,'shared source spans must stay shared');
  const generated=generatedNativeTerms(native);
  for(let index=0;index<generated.length;index++)canonicalContract(native,generated[index],'generated '+index);
  // Real loaded Book including Base, then bounded deterministic lowering samples.
  fs.writeFileSync(root,'import Base\n\ndef main() -> U32:\n  1\n');
  const book=native.book_nil();
  await native.book_load(book,root,'',new Map());native.book_valid(book);
  const keys=book.order.slice();
  const chosen=new Set<string>();
  for(const key of keys.slice(0,24))chosen.add(key);
  const step=Math.max(1,Math.floor(keys.length/12));
  for(let index=0;index<keys.length;index+=step)chosen.add(keys[index]);
  for(const pattern of [/Either$/,/Sigma$/,/App$/,/Equal\./,/List$/])for(const key of keys)if(pattern.test(key))chosen.add(key);
  const samples:Array<[string,HTerm]>=[];
  for(const key of keys)if(chosen.has(key)) {
    const declaration=book.tlds[key];
    if(declaration.$==='ADT'){samples.push([key+'.type',declaration.T]);for(const constructor of declaration.c)samples.push([key+'.'+constructor.k,constructor.T]);}
    else {samples.push([key+'.type',declaration.T]);if(declaration.v!==null)samples.push([key+'.value',declaration.v]);}
  }
  const before=[...bookChunks(native,book)];
  for(const [label,term] of samples) {
    const lowered=native.term_lower(term),expected=native.term_key(lowered);
    const shape=JSON.stringify(projection(lowered)),spans=sourceSpans(lowered);
    const relowered=JSON.stringify(projection(native.term_lower(term)));
    assert.equal(canonicalTermKey(lowered),expected,'book '+label+': canonical key differs from native term_key');
    assert.equal(native.term_key(lowered),expected,'book '+label+': native key changed after canonicalization');
    assert.equal(JSON.stringify(projection(lowered)),shape,'book '+label+': input structure was mutated');
    sameSourceSpans(sourceSpans(lowered),spans,'book '+label);
    assert.equal(JSON.stringify(projection(native.term_lower(term))),relowered,'book '+label+': source lowering was mutated');
    assert.equal(native.term_key(native.term_lower(term)),expected,'book '+label+': relowered native key changed');
  }
  const bookLowered=native.term_lower(book.tlds[keys[0]].T);
  const extendedLowered:ExtendedTerm=native.Ctr('BookExtended',[bookLowered],{file:{str:'book-lowering',ns:'contract',al:{}},beg:0,end:0});
  extendedLowered.semantic_extension={zulu:1,alpha:2,kept:[1,'two',true,null],label:'book lowering extra'};
  canonicalContract(native,extendedLowered,'book extended node');
  assert.deepEqual(extendedLowered.semantic_extension,{zulu:1,alpha:2,kept:[1,'two',true,null],label:'book lowering extra'},'Book lowering non-source fields must survive canonicalization');
  assert.equal(constructorChildren(extendedLowered)[0],bookLowered,'Book lowering aliases must be preserved');
  assert.deepEqual([...bookChunks(native,book)],before,'canonical keys must not change the Book stream');
  assert.deepEqual(book.order,keys,'canonical keys must not reorder the Book');
  console.log('Canonical term-key differential contracts passed over every native LTerm variant, deep source-field exclusion, escaping, generated terms and a Base-loaded Book');
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
async function replyFrameContract(){
  const home=path.join(fixture,'reply-frame');fs.mkdirSync(home);
  // 300 typed declarations keep the source under the input cap while the reply
  // (declarations, occurrences and targets) far exceeds any piped stdout buffer.
  const prefix='import Base\n\n';
  let offset=Buffer.byteLength(prefix);
  const targets=new Map<number,{file:string,start:number,end:number}>();
  const source=prefix+Array.from({length:300},(_,index)=>{
    const name=`f${index}`,declaration=`def ${name}() -> U32:\n  ${index%10}\n`;
    targets.set(offset+4,{file:'main.bend',start:offset+4,end:offset+4+name.length});
    offset+=Buffer.byteLength(declaration);
    return declaration;
  }).join('');
  const request={schema:1,id:'large-frame-λ🦀',root:'main.bend',files:{'main.bend':source},revisions:{'main.bend':1},inspect_base:[]};
  const followup={...request,id:'large-frame-followup-λ🦀'};
  const inputs=path.join(home,'inputs');fs.mkdirSync(inputs);
  const requestFiles=[path.join(inputs,'request-0.json'),path.join(inputs,'request-1.json')];
  fs.writeFileSync(requestFiles[0],JSON.stringify(request));fs.writeFileSync(requestFiles[1],JSON.stringify(followup));
  const frames=path.join(home,'frames');fs.mkdirSync(frames);
  // Bun/Node spawn "pipe" is a Unix socketpair that buffers the whole frame and
  // hides stdout loss. The helper owns a real os.pipe() bounded to 8192 bytes
  // and runs the authenticated worker with it as stdout, stdin left open.
  const helper=fileURLToPath(new URL('./semantic_frame_pipe.py',import.meta.url));
  const python=process.argv[3];
  assert.ok(python&&path.isAbsolute(python),'native contracts require the invoking Python executable');
  const invoked=spawnSync(python,[helper,process.execPath,path.join(installation,'worker.ts'),...requestFiles,frames],{cwd:home,encoding:'utf8',timeout:30000,maxBuffer:4000000});
  assert.equal(invoked.error,undefined);
  assert.equal(invoked.status,0,(invoked.stderr||'')+(invoked.stdout||''));
  const transport=JSON.parse(fs.readFileSync(path.join(frames,'transport.json'),'utf8'));
  const first=fs.readFileSync(path.join(frames,'frame-0.bin'));
  const second=fs.readFileSync(path.join(frames,'frame-1.bin'));
  // Fatal UTF-8 decoding proves the frame did not end inside a multibyte sequence.
  function reply(frame:Buffer,id:string){
    const value=JSON.parse(new TextDecoder('utf-8',{fatal:true}).decode(frame));
    assert.equal(value.schema,1);assert.equal(value.id,id);assert.equal(value.root,request.root);assert.equal(value.outcome,'valid');
    assert.equal(value.declarations.length,targets.size,'large reply must keep every declaration');
    const seen=new Set<number>();
    for(const declaration of value.declarations){
      const expected=targets.get(declaration.site.start);
      assert.ok(expected,'reply contains only the written declaration sites');
      assert.deepEqual(declaration.site,expected);
      assert.deepEqual(declaration.target,expected,'each function resolves to its own declaration');
      assert.equal(declaration.kind,'def');
      assert.ok(!seen.has(declaration.site.start),'written declaration must not be duplicated');
      seen.add(declaration.site.start);
    }
    assert.equal(value.occurrences.length,600,'each function keeps its binding and U32 reference');
    assert.ok(value.occurrences.every((occurrence:any)=>occurrence.status==='resolved'&&occurrence.target),'large reply must resolve every written occurrence');
    assert.equal(value.complete,true,'large reply must report complete resolution');
  }
  assert.equal(transport.exit,0,JSON.stringify(transport));
  assert.ok(first.length>65536,'reply frame must exceed a bounded pipe capacity: '+first.length);
  reply(first,request.id);
  assert.equal(transport.frames[0].trailing,0,'reply must be exactly one newline-terminated frame');
  assert.ok(second.length>65536,'follow-up reply frame must stay complete: '+second.length);
  reply(second,followup.id);
  assert.equal(transport.frames[1].trailing,0,'follow-up reply must be exactly one newline-terminated frame');
  if(transport.capacity!==null)assert.ok(first.length>transport.capacity,'frame must exceed the real OS pipe capacity: '+first.length+' vs '+transport.capacity);
  if(transport.fd1!==null)assert.ok(transport.fd1.startsWith('pipe:'),'worker stdout must be a real OS pipe: '+transport.fd1);
  console.log('Persistent worker lossless UTF-8 JSONL reply contract passed: '+first.length+' byte frame over a bounded OS pipe ('+String(transport.capacity??'capacity unreported')+'), open stdin and same-process follow-up');
}
try {
  const root=path.join(fixture,'main.bend');
  await nativeBookControls(root);
  await nativeCanonicalContracts(root);
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
  await replyFrameContract();
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
