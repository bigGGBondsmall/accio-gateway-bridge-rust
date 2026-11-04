'use strict';
// Synthetic loopback regression coverage for generation transport handling.
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const zlib = require('node:zlib');
const binary = path.resolve(process.argv[2] || path.join(__dirname, '../target/debug/accio-gateway-bridge-rust'));
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'accio-stream-'));
let child, provider;
const seenHeaders = [];
const frames = text => text.split('\n').filter(line => line.startsWith('data:')).map(line => JSON.parse(line.slice(5)));
async function main() {
  provider = http.createServer(async (req, res) => {
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    const body = JSON.parse(Buffer.concat(chunks).toString() || '{}');
    if (req.url.startsWith('/relay-probe')) { seenHeaders.push({ path: req.url, headers: req.headers }); res.setHeader('content-type', 'application/json'); res.end('{"ok":true}'); return; }
    if (req.url !== '/v1/chat/completions' || body.model === 'html') {
      res.setHeader('content-type', 'text/html'); res.setHeader('content-encoding', 'gzip');
      res.end(zlib.gzipSync('<html><script>DO_NOT_RENDER</script><h1>Gateway unavailable</h1></html>')); return;
    }
    if (body.model === 'thoughtjson') { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({choices:[{message:{reasoning_content:'thinking'},finish_reason:'stop'}]})); return; }
    if (body.model === 'jsonempty') { res.setHeader('content-type', 'application/json'); res.end('{}'); return; }
    if (body.model === 'json') {
      res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({choices:[{message:{content:'JSON fallback',reasoning_content:'thinking'},finish_reason:'stop'}]})); return;
    }
    res.setHeader('content-type', 'text/event-stream');
    const events = body.model === 'empty' ? [] : [
      {choices:[{delta:{reasoning_content:'thinking'}}]},
      {choices:[{delta:{tool_calls:[{index:0,id:'first',function:{name:'read',arguments:'{"x":1}'}}]}}]},
      {choices:[{delta:{tool_calls:[{index:0,id:'second',function:{name:'read',arguments:'{"x":2}'}}]},finish_reason:'tool_calls'}]},
    ];
    res.end(events.map(event => `data: ${JSON.stringify(event)}\n\n`).join('')+'data: [DONE]\n\n');
  });
  provider.listen(0, '127.0.0.1'); await once(provider, 'listening');
  const upstream = `http://127.0.0.1:${provider.address().port}`;
  fs.writeFileSync(path.join(dir, 'config.json'), JSON.stringify({server:{host:'127.0.0.1',port:0,token:'gateway-secret'},upstream:{baseUrl:upstream},models:['json','jsonempty','thoughtjson','html','tools','empty'].map(id=>({id,provider:'openai',baseUrl:upstream+'/v1',upstreamModel:id}))}));
  child = spawn(binary, ['--config',path.join(dir,'config.json'),'--port','0'], {stdio:['ignore','pipe','pipe']});
  const port = await new Promise((resolve,reject) => {
    let buffer = ''; const timer = setTimeout(()=>reject(new Error('startup timeout')),15000);
    child.on('error', reject); child.stdout.on('data', chunk => {
      buffer += chunk; const line = buffer.split('\n')[0];
      try { const event=JSON.parse(line); if(event.event==='listening'){clearTimeout(timer);resolve(event.port);} } catch {}
    });
  });
  const base = `http://127.0.0.1:${port}`;
  const generate = async (model,extra={}) => {
    const response = await fetch(base+'/api/adk/llm/generateContent',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({model,contents:[{parts:[{text:'test'}]}],...extra}),signal:AbortSignal.timeout(5000)});
    assert.equal(response.status,200); assert.match(response.headers.get('content-type'), /text\/event-stream/);
    return frames(await response.text());
  };
  for(const stream of [true,false]) {
    const result = await generate('json',{stream,generationConfig:{thinkingConfig:{includeThoughts:false}}});
    assert.ok(JSON.stringify(result).includes('JSON fallback')); assert.ok(!JSON.stringify(result).includes('thinking'));
    const errors=await generate('html',{stream}); assert.match(errors[0].error.message,/Gateway unavailable/); assert.ok(!JSON.stringify(errors).includes('DO_NOT_RENDER'));
  }
  for (const stream of [true,false]) { assert.match((await generate('jsonempty',{stream}))[0].error.message,/completion message/); }
  for (const stream of [true,false]) {
    const hidden=await generate('thoughtjson',{stream,includeThoughts:false}); assert.ok(hidden[0].error); assert.ok(!hidden.some(frame=>frame.finishReason==='STOP')); 
    assert.equal((await generate('thoughtjson',{stream}))[0].content.parts[0].thought,true);
  }
  const calls=await generate('tools',{generationConfig:{thinkingBudget:0}});
  assert.ok(!JSON.stringify(calls).includes('thinking'));
  assert.deepEqual(calls.at(-1).content.parts.map(p=>p.functionCall.id),['first','second']);
  const thought=await generate('tools'); assert.equal(thought[0].content.parts[0].thought,true);
  assert.match((await generate('empty'))[0].error.message,/empty response/);
  const official=await generate('official'); assert.match(official[0].error.message,/Gateway unavailable/);
  const ordinary=await fetch(base+'/ordinary'); assert.match(ordinary.headers.get('content-type'),/text\/html/); assert.ok((await ordinary.text()).includes('DO_NOT_RENDER'));
  // Node parity (proxy.js): the official passthrough relays Accio's own headers on every path —
  // the 1688 workflows need POST /api/mcp/ali1688/sgcookie to reach the origin with its session —
  // but this gateway's management token must never leave.
  await fetch(base+'/relay-probe/one',{headers:{cookie:'sgcookie=SYNTHETIC',authorization:'Bearer accio-session','x-accio-token':'accio-token','x-gateway-token':'must-not-leak'}});
  const forwarded=seenHeaders.at(-1).headers;
  assert.equal(forwarded.cookie,'sgcookie=SYNTHETIC','cookie must reach the official origin');
  assert.equal(forwarded.authorization,'Bearer accio-session','inbound authorization must reach the official origin');
  assert.equal(forwarded['x-accio-token'],'accio-token','the Accio token header must be relayed');
  assert.equal(forwarded['x-gateway-token'],undefined,'the local management token must never be forwarded');
  await fetch(base+'/relay-probe/two',{headers:{cookie:'sgcookie=SYNTHETIC',authorization:'Bearer gateway-secret'}});
  const dropped=seenHeaders.at(-1).headers;
  assert.equal(dropped.authorization,undefined,'the saved gateway token must not be relayed');
  assert.equal(dropped.cookie,'sgcookie=SYNTHETIC','cookies still relay on other paths');
  console.log('PASS streaming thoughts, reused tool indexes, empty stream, JSON fallback, compressed HTML guards, transparent ordinary proxy and credential relay');
}
main().catch(error=>{console.error(error);process.exitCode=1;}).finally(async()=>{if(child){child.kill('SIGTERM');await once(child,'exit').catch(()=>{});}if(provider)await new Promise(resolve=>provider.close(resolve));fs.rmSync(dir,{recursive:true,force:true});});
