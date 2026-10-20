// One persistent pristine history, one fresh Book per project, one credited batch at a time.
import {parentPort} from 'node:worker_threads';
import {edenGC,gcAndSweep} from 'bun:jsc';
import * as Plain from './bend.ts';
import {bookChunks,BATCH_CHUNKS,BATCH_CHARS} from './semantic_book_stream.ts';
const IDLE_HEAP_BYTES=32*1024*1024;
if(!parentPort)throw Error('missing-pristine-parent');
const port=parentPort;
let networkAttempted=false;
globalThis.fetch=async()=>{networkAttempted=true;throw Error('network-forbidden');};
let active:any=null,lastRequest=0,busy=false;
function fatal(error:any) {
  port.postMessage({kind:'fatal',request:active?.request??lastRequest,error:String(error)});
  active=null;port.close();
}
port.on('message',async(message:any)=>{
  try {
    if(busy)throw Error('pristine-overlapping-request');
    if(message?.kind==='start'){
      if(active||!Number.isSafeInteger(message.request)||message.request!==lastRequest+1||typeof message.root!=='string')throw Error('pristine-start-protocol-failure');
      // The previous callback has returned and no longer owns a live Book.
      if(lastRequest>0&&edenGC()>IDLE_HEAP_BYTES)gcAndSweep();
      busy=true;lastRequest=message.request;networkAttempted=false;
      const book=Plain.book_nil(),metrics:Record<string,number>={};let error:any=null,stage=performance.now();
      try {
        await Plain.book_load(book,message.root,'',new Map());
        metrics.parity_load_seconds=(performance.now()-stage)/1000;stage=performance.now();
        Plain.book_valid(book);metrics.parity_check_seconds=(performance.now()-stage)/1000;
      }catch(caught){error=caught;}
      if(networkAttempted)throw Error('network-forbidden');
      if(error&&error.$!=='Err')throw error;
      active={request:message.request,book,iterator:error?null:bookChunks(Plain,book),sequence:0,canonicalSeconds:0};
      busy=false;
      port.postMessage({kind:'loaded',request:message.request,error:error?Plain.err_show(error):null,holes:book.hols,count:book.order.length,metrics});
      return;
    }
    if(!active||message?.request!==active.request)throw Error('pristine-stale-request');
    if(message.kind==='finish'){
      if(active.iterator)throw Error('pristine-premature-finish');
      active=null;port.postMessage({kind:'finished',request:message.request});return;
    }
    if(message.kind!=='next'||!active.iterator||message.sequence!==active.sequence)throw Error('pristine-credit-protocol-failure');
    const stage=performance.now(),chunks=[];let eof=false,chars=0;
    for(let index=0;index<BATCH_CHUNKS;index++){
      const next=active.pending??active.iterator.next();active.pending=null;
      if(next.done){eof=true;break;}
      if(chars+next.value[2].length>BATCH_CHARS){active.pending=next;break;}
      chunks.push(next.value);chars+=next.value[2].length;
    }
    active.canonicalSeconds+=(performance.now()-stage)/1000;
    const reply={kind:'batch',request:active.request,sequence:active.sequence++,chunks,eof,holes:active.book.hols,count:active.book.order.length,canonical_seconds:active.canonicalSeconds};
    if(eof)active=null;
    port.postMessage(reply);
  }catch(error){busy=false;fatal(error);}
});
