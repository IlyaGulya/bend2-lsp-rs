// Exact native canonical components; never serialize an entire Book or replace HTerm lowering.
export const CHUNK_CHARS=16384;
export const BATCH_CHUNKS=256;
export const BATCH_CHARS=65536;
export type Chunk=[number,number,string,boolean];
// Lowered leaves may alias a live Book. Copy only paths that contain a source
// field; native JSON handles ordering, escaping and scalar formatting unchanged.
function withoutSources(value:unknown):unknown {
  if(value===null||typeof value!=='object')return value;
  if(Array.isArray(value)){
    const values:readonly unknown[]=value;
    let copy:unknown[]|undefined;
    for(let index=0;index<values.length;index++){
      const child=values[index];
      if(child===null||typeof child!=='object')continue;
      const clean=withoutSources(child);
      if(clean!==child){copy??=values.slice();copy[index]=clean;}
    }
    return copy??value;
  }
  // Object shape is native data; own string properties remain unknown until read.
  const fields=value as Record<string,unknown>;
  let copy:Record<string,unknown>|undefined;
  for(const key in fields){
    if(!Object.hasOwn(fields,key))continue;
    const child=fields[key];
    if(key==='s'){
      if(child!==undefined){copy??={...value};copy.s=undefined;}
      continue;
    }
    if(child===null||typeof child!=='object')continue;
    const clean=withoutSources(child);
    if(clean!==child){copy??={...value};copy[key]=clean;}
  }
  return copy??value;
}
export function canonicalTermKey(lowered:unknown):string {
  const key=JSON.stringify(withoutSources(lowered));
  if(typeof key!=='string')throw Error('invalid-native-canonical-key');
  return key;
}
function* components(native:any,book:any):Generator<string> {
  yield JSON.stringify(['Book',book.order.length,book.hols]);
  for(const key of book.order){
    const declaration=book.tlds[key];
    if(!declaration||(declaration.$!=='Def'&&declaration.$!=='ADT'))throw Error('invalid-native-declaration');
    yield JSON.stringify([key,declaration.$,declaration.$==='Def'?declaration.v!==null:declaration.c.length]);
    yield canonicalTermKey(native.term_lower(declaration.T));
    if(declaration.$==='Def'){
      if(declaration.v!==null)yield canonicalTermKey(native.term_lower(declaration.v));
    }else for(const constructor of declaration.c){
      yield constructor.k;
      yield canonicalTermKey(native.term_lower(constructor.T));
    }
  }
}
export function* bookChunks(native:any,book:any):Generator<Chunk> {
  let component=0;
  for(const value of components(native,book)){
    if(typeof value!=='string')throw Error('invalid-native-canonical-key');
    for(let offset=0;offset<value.length||offset===0;offset+=CHUNK_CHARS){
      yield [component,offset,value.slice(offset,offset+CHUNK_CHARS),offset+CHUNK_CHARS>=value.length];
    }
    component++;
  }
}
export function validateBatch(message:any,sequence:number,request:number) {
  if(message?.kind!=='batch'||message.request!==request||message.sequence!==sequence||typeof message.eof!=='boolean'||!Array.isArray(message.chunks)||message.chunks.length>BATCH_CHUNKS||(!message.eof&&message.chunks.length===0))throw Error('pristine-batch-protocol-failure');
  let chars=0;
  for(const chunk of message.chunks){
    if(!Array.isArray(chunk)||chunk.length!==4||!Number.isSafeInteger(chunk[0])||chunk[0]<0||!Number.isSafeInteger(chunk[1])||chunk[1]<0||typeof chunk[2]!=='string'||chunk[2].length>CHUNK_CHARS||typeof chunk[3]!=='boolean')throw Error('pristine-chunk-protocol-failure');
    chars+=chunk[2].length;
  }
  if(chars>BATCH_CHARS)throw Error('pristine-batch-limit');
}
export function compareBatch(iterator:Generator<Chunk>,message:any) {
  for(const chunk of message.chunks){
    const expected=iterator.next();
    if(expected.done||expected.value[0]!==chunk[0]||expected.value[1]!==chunk[1]||expected.value[2]!==chunk[2]||expected.value[3]!==chunk[3])throw Error('instrumentation-target-parity-failure');
  }
  if(message.eof&&!iterator.next().done)throw Error('instrumentation-target-parity-failure');
}
