'use strict';
// Black-box Rust core contract checks. All providers are synthetic loopback servers.
// Usage: node tests/parity.cjs [path-to-built-binary]
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const zlib = require('node:zlib');
const binary = path.resolve(process.argv[2] || path.join(__dirname, '../target/debug/accio-gateway-bridge-rust'));
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'accio-rust-parity-'));
let child, provider, total = 0, failures = 0;
const captured = [];
async function check(name, fn) { total++; try { await fn(); console.log(`PASS ${name}`); } catch (error) { failures++; console.error(`FAIL ${name}: ${error.stack}`); } }
function frames(body) { return body.split(/\r?\n/).filter(s => s.startsWith('data:')).map(s => JSON.parse(s.slice(5).trim())); }
async function main() {
  provider = http.createServer(async (req, res) => {
    const chunks = []; for await (const c of req) chunks.push(c);
    const raw = Buffer.concat(chunks); let body; try {body = JSON.parse(raw);} catch {body = {};}
    captured.push({ path: req.url, method: req.method, body, raw: raw.toString(), headers: req.headers });
    if (req.url === '/api/llm/config/v2') { res.setHeader('content-type','application/json'); res.end(JSON.stringify({success:true,code:'200',data:[{provider:'alibaba',modelList:[{modelCode:'official'}]}]})); return; }
    if (req.url === '/v1/chat/completions') {
      if (body.model === 'truncate') { res.setHeader('content-type','text/event-stream');res.end('data: {"choices":[{"delta":{"content":"partial"}}]}\n\n'); return; }
      if (body.model === 'tool') { res.setHeader('content-type','text/event-stream');res.end('data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"provider_call","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}\n\ndata: [DONE]\n\n'); return; }
      if (!body.stream) { res.setHeader('content-type','application/json'); res.end(JSON.stringify({choices:[{message:{content:'nonstream'},finish_reason:'stop'}],usage:{prompt_tokens:2,completion_tokens:3}})); return; }
      if (body.model === 'toolcut') { res.setHeader('content-type','text/event-stream');res.end('data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"cut","function":{"name":"read","arguments":"{\\"path\\":\\"a.tx"}}]}}]}\n\ndata: {"choices":[{"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":5,"completion_tokens":9}}\n\ndata: [DONE]\n\n'); return; }
      if (body.model === 'toolusage') { res.setHeader('content-type','text/event-stream');res.end('data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"kept","function":{"name":"read","arguments":"{\\"path\\":\\"a.txt\\"}"}}]}}]}\n\ndata: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":9}}\n\ndata: [DONE]\n\n'); return; }
      res.setHeader('content-type','text/event-stream');res.end('data: {"choices":\r\ndata: [{"delta":{"content":"hello"}}]}\r\n\r\ndata: [DONE]\r\n\r\n'); return;
    }
    if (req.url === '/api/chat') { res.setHeader('content-type','application/x-ndjson');res.end('{"message":{"content":"local"},"done":true,"prompt_eval_count":2,"eval_count":3}'); return; }
    res.statusCode=207;res.setHeader('x-upstream','synthetic');res.end(raw);
  });
  provider.listen(0,'127.0.0.1'); await once(provider,'listening');
  const upstream=`http://127.0.0.1:${provider.address().port}`;
  const config={server:{host:'127.0.0.1',port:0},upstream:{baseUrl:upstream},catalog:{fetchOfficial:true},models:[
    {id:'openai',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'normal',apiKey:'synthetic-secret',enabled:true,aliases:['alias']},
    {id:'ollama',provider:'ollama',baseUrl:upstream,upstreamModel:'local',enabled:true},
    {id:'toolcut',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'toolcut',enabled:true},
    {id:'off',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'normal',enabled:false},
    {id:'toolusage',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'toolusage',enabled:true},
    {id:'tool',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'tool',enabled:true},
    {id:'truncate',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'truncate',enabled:true},
  ]};
  fs.writeFileSync(path.join(dir,'config.json'),JSON.stringify(config));
  const captureDir=path.join(dir,'captured');
  child=spawn(binary,['--config',path.join(dir,'config.json'),'--port','0'],{stdio:['ignore','pipe','pipe'],env:{...process.env,ACCIO_BRIDGE_CAPTURE_DIR:captureDir}});
  let stderr='';child.stderr.on('data',c=>stderr+=c);
  const port=await new Promise((resolve,reject)=>{
    let out='';const timeout=setTimeout(()=>reject(new Error('startup timeout '+stderr)),15000);
    child.on('error',e=>{clearTimeout(timeout);reject(e);});child.on('exit',code=>{clearTimeout(timeout);reject(new Error(`premature exit ${code}: ${stderr}`));});
    child.stdout.on('data',c=>{out+=c;let i;while((i=out.indexOf('\n'))>=0){const line=out.slice(0,i);out=out.slice(i+1);try{const msg=JSON.parse(line);if(msg.event==='listening'){clearTimeout(timeout);resolve(msg.port);}}catch{}}});
  });
  const base=`http://127.0.0.1:${port}`;
  const req=(route,method='GET',body,headers={})=>fetch(base+route,{method,headers:{...(body!==undefined?{'content-type':'application/json'}:{}),...headers},body:body===undefined?undefined:JSON.stringify(body),signal:AbortSignal.timeout(5000)});
  const generate=async(model,extra={})=>{const response=await req('/api/adk/llm/generateContent','POST',{model,contents:[{parts:[{text:'test'}]}],...extra});assert.equal(response.status,200);return frames(await response.text());};
  await check('health contract',async()=>{const r=await req('/healthz');assert.equal(r.status,200);assert.equal((await r.json()).ok,true);});
  await check('catalog merges custom models before official providers',async()=>{const r=await req('/api/llm/config/v2','POST',{});assert.equal(r.status,200);const v=await r.json();assert.equal(v.data[0].provider,'custom');assert.equal(v.data[0].modelList[0].modelCode,'openai');assert.equal(v.data[1].provider,'alibaba');});
  await check('status redacts credentials',async()=>{const s=await(await req('/api/bridge/status')).text();assert.ok(!s.includes('synthetic-secret'));assert.equal(JSON.parse(s).models[0].hasKey,true);});
  await check('OpenAI multiline SSE and aliases',async()=>{const f=await generate('ALIAS');assert.equal(f[0].content.parts[0].text,'hello');assert.equal(f.at(-1).turnComplete,true);});
  await check('Ollama terminal line without newline',async()=>{const f=await generate('ollama');assert.ok(JSON.stringify(f).includes('local'));assert.equal(f.at(-1).turnComplete,true);});
  await check('playground test prompt reaches provider messages',async()=>{const r=await req('/api/bridge/test','POST',{model:'openai',prompt:'synthetic playground'});assert.equal(r.status,200);await r.text();assert.ok(captured.at(-1).body.messages.some(m=>m.role==='user'&&m.content==='synthetic playground'));});
  await check('nonstream output still uses GenAI frames',async()=>{const f=await generate('openai',{stream:false});assert.equal(f[0].content.parts[0].text,'nonstream');assert.equal(f[0].usageMetadata.totalTokenCount,5);});
  await check('upstream tool ID survives response',async()=>{const f=await generate('tool');const call=f.flatMap(x=>x.content?.parts||[]).find(x=>x.functionCall).functionCall;assert.equal(call.id,'provider_call');});
  await check('truncated SSE is not successful STOP',async()=>{const f=await generate('truncate');assert.ok(f.some(x=>x.error));assert.ok(!f.some(x=>x.finishReason==='STOP'));});
  await check('gzip request accepted with and without encoding header',async()=>{const data=zlib.gzipSync(Buffer.from(JSON.stringify({model:'openai',contents:[{parts:[{text:'gzip'}]}]})));for(const headers of [{'content-encoding':'gzip'},{}]){const r=await fetch(base+'/api/adk/llm/generateContent',{method:'POST',headers:{'content-type':'application/json',...headers},body:data,signal:AbortSignal.timeout(5000)});assert.equal(r.status,200);assert.ok((await r.text()).includes('hello'));}});
  await check('tool continuation matches generated call IDs',async()=>{await generate('openai',{contents:[{role:'model',parts:[{functionCall:{name:'read'}}]},{parts:[{functionResponse:{name:'read',response:{ok:true}}}]}]});const m=captured.at(-1).body.messages;assert.equal(m[1].tool_call_id,m[0].tool_calls[0].id);});
  await check('inline image reaches compatible provider',async()=>{await generate('openai',{contents:[{parts:[{inlineData:{mimeType:'image/png',data:'YWJj'}}]}]});assert.equal(captured.at(-1).body.messages[0].content[0].image_url.url,'data:image/png;base64,YWJj');});
  await check('official passthrough preserves status/body/query',async()=>{const r=await req('/unknown?q=1','POST',{a:'unchanged'});assert.equal(r.status,207);assert.equal(r.headers.get('x-upstream'),'synthetic');assert.deepEqual(await r.json(),{a:'unchanged'});assert.equal(captured.at(-1).path,'/unknown?q=1');});
  await check('malicious Origin blocked',async()=>{assert.equal((await req('/api/bridge/status','GET',undefined,{Origin:'https://evil.example'})).status,403);});
  await check('model CRUD persists without exposing API key',async()=>{const r=await req('/api/bridge/models','POST',{id:'added',provider:'openai',baseUrl:upstream+'/v1',apiKey:'fresh-secret'});assert.equal(r.status,200);assert.ok(!(await r.text()).includes('fresh-secret'));assert.ok(JSON.parse(fs.readFileSync(path.join(dir,'config.json'))).models.some(m=>m.id==='added'));assert.equal((await req('/api/bridge/models?id=added','DELETE')).status,200);});
  await check('provider groups add models without reading saved credentials',async()=>{
    const created=await req('/api/bridge/providers','POST',{provider:'openai',baseUrl:upstream+'/v1',apiKey:'group-secret',models:[{id:'group-first',upstreamModel:'normal'}]});
    assert.equal(created.status,200);const value=await created.json();assert.ok(!JSON.stringify(value).includes('group-secret'));const group=value.provider;
    assert.ok(group.id);assert.equal(group.apiKeyMasked,'••••');
    const appended=await req('/api/bridge/providers/'+encodeURIComponent(group.id)+'/models','POST',{models:[{id:'group-second'}]});assert.equal(appended.status,200);
    const persisted=JSON.parse(fs.readFileSync(path.join(dir,'config.json')));assert.equal(persisted.models.find(m=>m.id==='group-second').apiKey,'group-secret');
    const toggle=await req('/api/bridge/providers/'+encodeURIComponent(group.id),'PATCH',{enabled:false});assert.equal(toggle.status,200);assert.equal((await toggle.json()).provider.enabled,false);
    assert.equal((await req('/api/bridge/providers/'+encodeURIComponent(group.id),'DELETE')).status,200);
  });
  await check('model PATCH handles encoded slash IDs and mutually exclusive defaults',async()=>{
    assert.equal((await req('/api/bridge/models','POST',{id:'compound/model',provider:'openai',baseUrl:upstream+'/v1',enabled:true})).status,200);
    const updated=await req('/api/bridge/models/compound%2Fmodel','PATCH',{displayName:'Updated',isDefault:true});assert.equal(updated.status,200);
    const models=(await(await req('/api/bridge/models')).json()).models;assert.equal(models.find(m=>m.id==='compound/model').displayName,'Updated');assert.equal(models.filter(m=>m.isDefault).length,1);
  });
  await check('legacy POST delete supports body IDs and query fallback',async()=>{
    for(const route of ['/api/bridge/models/delete','/__bridge/api/models/delete/']) {
      assert.equal((await req('/api/bridge/models','POST',{id:'legacy/model',baseUrl:upstream+'/v1'})).status,200);
      const removed=await req(route,'POST',{id:'LEGACY/model'});assert.equal(removed.status,200);assert.equal((await removed.json()).deleted,'LEGACY/model');
      assert.ok(!JSON.parse(fs.readFileSync(path.join(dir,'config.json'))).models.some(m=>m.id==='legacy/model'));
      assert.equal((await req('/api/bridge/models','POST',{id:'query/model',baseUrl:upstream+'/v1'})).status,200);
      assert.equal((await req(route+'?id=query%2Fmodel','POST')).status,200);
      assert.equal((await req(route,'POST',{})).status,400);
      assert.equal((await req(route,'POST',{id:123})).status,400);
    }
  });
  await check('path-style model delete removes the model',async()=>{
    assert.equal((await req('/api/bridge/models','POST',{id:'path/deleted',provider:'openai',baseUrl:upstream+'/v1'})).status,200);
    assert.equal((await req('/api/bridge/models/path%2Fdeleted','DELETE')).status,200);
    assert.ok(!JSON.parse(fs.readFileSync(path.join(dir,'config.json'))).models.some(m=>m.id==='path/deleted'));
    assert.equal((await req('/api/bridge/models/path%2Fdeleted','DELETE')).status,404);
    assert.equal((await req('/api/bridge/models/missing','DELETE')).status,404);
  await check('management panel is served by the gateway on every legacy path',async()=>{
    for(const route of ['/','/index.html','/dashboard','/dashboard/']){
      const r=await req(route);
      assert.equal(r.status,200,route);
      assert.match(r.headers.get('content-type'),/text\/html/);
      const html=await r.text();
      assert.ok(html.includes('Accio Gateway Bridge · Dashboard & Playground'),route);
      // The panel must keep the redaction-aware key handling; sending an untouched key field
      // as an empty string would clear a stored credential.
      assert.ok(html.includes('primeApiKeyField'),route);
      assert.ok(html.includes('apiKeyFieldDirty'),route);
    }
    // The panel shadows the old passthrough for these paths only: an unrelated route still proxies.
    assert.notEqual((await req('/api/bridge/status')).headers.get('content-type'),'text/html; charset=utf-8');
  });
  await check('panel key handling preserves a stored key and allows an explicit clear',async()=>{
    assert.equal((await req('/api/bridge/models','POST',{id:'panel/keyed',provider:'openai',baseUrl:upstream+'/v1',apiKey:'panel-secret'})).status,200);
    const withKey=async()=>{const l=(await(await req('/api/bridge/models')).json()).models.find(m=>m.id==='panel/keyed');return l?l.hasKey:null;};
    assert.equal(await withKey(),true);
    // Untouched field => the property is omitted => the stored key survives a rename.
    await req('/api/bridge/models','POST',{id:'panel/keyed',displayName:'renamed'});
    assert.equal(await withKey(),true);
    // Explicit empty string still clears it.
    await req('/api/bridge/models','POST',{id:'panel/keyed',apiKey:''});
    assert.equal(await withKey(),false);
    assert.equal((await req('/api/bridge/models/panel%2Fkeyed','DELETE')).status,200);
  });
  });
  await check('opt-in capture writes the request and the tool set is logged',async()=>{
    await generate('openai',{contents:[{parts:[{text:'tools'}]}],tools:[{functionDeclarations:[
      {name:'placeholder'},
      {name:'placeholder',description:'real',parameters_json:'{"type":"object","properties":{}}'},
      {name:'permissive'}
    ]}]});
    const captured=fs.readdirSync(captureDir).filter(f=>f.startsWith('generate-'));
    assert.ok(captured.length>0,`nothing captured in ${captureDir}`);
    const body=JSON.parse(fs.readFileSync(path.join(captureDir,captured.at(-1)),'utf8'));
    assert.ok(JSON.stringify(body).includes('placeholder'));
    const report=await(await req('/api/bridge/logs?level=info&lines=50')).json();
    assert.strictEqual(report.ok,true,JSON.stringify(report).slice(0,200));
    assert.strictEqual(report.total,report.lines.length);
    assert.ok(report.lines.length>0,'no log lines');
    // The Node gateway stores {ts, level, msg} (server.js pushLogBuffer) and the desktop log view
    // renders exactly line.ts / line.level / line.msg; a renamed field silently empties that view.
    for(const line of report.lines){
      assert.strictEqual(typeof line.ts,'string',JSON.stringify(line));
      assert.strictEqual(typeof line.level,'string',JSON.stringify(line));
      assert.strictEqual(typeof line.msg,'string',JSON.stringify(line));
      assert.ok(!('timestamp' in line)&&!('message' in line),JSON.stringify(line));
    }
    const logs=report.lines.map(l=>l.msg).join(' | ');
    assert.ok(logs.includes('tools forwarded for openai'),logs);
  });
  await check('recording level switches at runtime and gates debug lines',async()=>{
    const initial=await(await req('/api/bridge/logging')).json();
    assert.strictEqual(initial.ok,true);
    assert.deepStrictEqual(initial.levels,['debug','info','warn','error']);
    assert.strictEqual((await req('/api/bridge/logging','POST',{level:'verbose'})).status,400);
    const set=await(await req('/api/bridge/logging','POST',{level:'debug'})).json();
    assert.strictEqual(set.level,'debug');
    await generate('openai');
    const debugLines=(await(await req('/api/bridge/logs?lines=100')).json()).lines.filter(l=>l.level==='debug');
    assert.ok(debugLines.some(l=>l.msg.includes('generate openai ->')),JSON.stringify(debugLines.slice(0,3)));
    assert.ok(debugLines.some(l=>l.msg.startsWith('upstream replied status=200')),JSON.stringify(debugLines.slice(0,3)));
    await generate('openai',{contents:[{role:'model',parts:[{functionCall:{name:'read'}}]},{parts:[{functionResponse:{name:'read',response:{ok:true}}}]}]});
    const fed=(await(await req('/api/bridge/logs?lines=100')).json()).lines.filter(l=>l.level==='debug'&&l.msg.startsWith('tool results fed back: read'));
    assert.ok(fed.length>0,'a tool continuation must log which results were fed back');
    const back=await(await req('/api/bridge/logging','POST',{level:'info'})).json();
    assert.strictEqual(back.level,'info');
    const count=async()=>(await(await req('/api/bridge/logs?lines=200')).json()).lines.filter(l=>l.level==='debug').length;
    const before=await count();
    await generate('openai');
    assert.strictEqual(await count(),before,'no new debug lines after switching back to info');
  });
  await check('a length-limited tool call is reported instead of delivered',async()=>{
    const f=await generate('toolcut');
    assert.ok(f.some(x=>x.error&&/length/.test(x.error.message)),JSON.stringify(f));
    assert.ok(!f.some(x=>x.finishReason==='TOOL_CALLS'),'a truncated call must not look runnable');
    assert.ok(!f.some(x=>JSON.stringify(x).includes('functionCall')),'the half-written call must not be delivered');
    assert.strictEqual(f.at(-1).usageMetadata.totalTokenCount,14,'usage survives the failure');
  });
  await check('a finished tool call keeps its usage',async()=>{
    const f=await generate('toolusage');
    assert.strictEqual(f.at(-1).finishReason,'TOOL_CALLS');
    assert.strictEqual(f.at(-1).usageMetadata.totalTokenCount,14);
    const call=f.flatMap(x=>x.content?.parts||[]).find(x=>x.functionCall).functionCall;
    assert.strictEqual(call.name,'read');
    assert.strictEqual(call.argsJson,'{"path":"a.txt"}');
  });
  await check('a disabled model is refused by name, an unknown one still proxies',async()=>{
    // A registered model that is switched off used to fall through to the official passthrough,
    // where the request failed for an unrelated reason. It is now named explicitly.
    const disabled=await req('/api/adk/llm/generateContent','POST',{model:'off',contents:[{parts:[{text:'hi'}]}]});
    assert.strictEqual(disabled.status,404);
    const body=await disabled.json();
    assert.match(body.error,/configured but disabled/);
    // An unknown name is still an official model as far as the gateway knows: it proxies.
    const unknown=await req('/api/adk/llm/generateContent','POST',{model:'qwen-max',contents:[{parts:[{text:'hi'}]}]});
    assert.strictEqual(unknown.status,207);
    // A configured alias keeps working.
    assert.strictEqual((await generate('alias'))[0].content.parts[0].text,'hello');
  });
  console.log(`${total-failures}/${total} black-box Rust core checks passed`);
  if (failures) process.exitCode = 1;
}
main().catch(e=>{console.error(e);process.exitCode=1;}).finally(async()=>{if(child && child.exitCode===null){child.kill('SIGTERM');await Promise.race([once(child,'exit'),new Promise(resolve=>setTimeout(resolve,2000))]);if(child.exitCode===null)child.kill('SIGKILL');}if(provider){provider.closeAllConnections();await new Promise(resolve=>provider.close(resolve));}fs.rmSync(dir,{recursive:true,force:true});});
