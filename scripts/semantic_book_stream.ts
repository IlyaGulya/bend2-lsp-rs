// Exact native canonical components; never serialize an entire Book or replace HTerm lowering.
export const CHUNK_CHARS=16384;
export const BATCH_CHUNKS=64;
export const BATCH_CHARS=65536;
export type Chunk=[number,number,string,boolean];
function* components(native:any,book:any):Generator<string> {
  yield JSON.stringify(['Book',book.order.length,book.hols]);
  for(const key of book.order){
    const declaration=book.tlds[key];
    if(!declaration||!['Def','ADT'].includes(declaration.$))throw Error('invalid-native-declaration');
    yield JSON.stringify([key,declaration.$,declaration.$==='Def'?declaration.v!==null:declaration.c.length]);
    yield native.term_key(native.term_lower(declaration.T));
    if(declaration.$==='Def'){
      if(declaration.v!==null)yield native.term_key(native.term_lower(declaration.v));
    }else for(const constructor of declaration.c){
      yield constructor.k;
      yield native.term_key(native.term_lower(constructor.T));
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
    if(expected.done||expected.value.some((value,index)=>value!==chunk[index]))throw Error('instrumentation-target-parity-failure');
  }
  if(message.eof&&!iterator.next().done)throw Error('instrumentation-target-parity-failure');
}
