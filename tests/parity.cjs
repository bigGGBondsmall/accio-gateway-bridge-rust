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
// Resolve the core binary the way it is actually built: the documented argument wins, then
// ACCIO_BRIDGE_TEST_BINARY, then CARGO_TARGET_DIR (this repo is built with an external target dir,
// so the artifact is NOT under <repo>/target), then the repo default. Windows artifacts carry
// .exe. The documented invocation `node tests/parity.cjs <path-to-binary>` is unchanged.
const exeSuffix = process.platform === 'win32' ? '.exe' : '';
const binary = (() => {
  const explicit = process.argv[2] || process.env.ACCIO_BRIDGE_TEST_BINARY;
  if (explicit) return path.resolve(explicit);
  const repoRoot = path.join(__dirname, '..');
  const candidates = [
    process.env.CARGO_TARGET_DIR && path.join(process.env.CARGO_TARGET_DIR, 'debug', `accio-gateway-bridge-rust${exeSuffix}`),
    process.env.CARGO_TARGET_DIR && path.join(process.env.CARGO_TARGET_DIR, 'release', `accio-gateway-bridge-rust${exeSuffix}`),
    path.join(repoRoot, 'target', 'debug', `accio-gateway-bridge-rust${exeSuffix}`),
    path.join(repoRoot, 'target', 'release', `accio-gateway-bridge-rust${exeSuffix}`),
  ].filter(Boolean);
  const found = candidates.find(candidate => fs.existsSync(candidate));
  if (found) return found;
  throw new Error(`cannot find the gateway binary; build it first (cargo build --locked) or set ACCIO_BRIDGE_TEST_BINARY. Tried:\n  ${candidates.join('\n  ')}`);
})();
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'accio-rust-parity-'));
let child, provider, total = 0, failures = 0;
// Gateways started by the credential-separation block, killed like the main one.
const extraChildren = [];
const captured = [];
async function check(name, fn) { total++; try { await fn(); console.log(`PASS ${name}`); } catch (error) { failures++; console.error(`FAIL ${name}: ${error.stack}`); } }
function frames(body) { return body.split(/\r?\n/).filter(s => s.startsWith('data:')).map(s => JSON.parse(s.slice(5).trim())); }
async function main() {
  provider = http.createServer(async (req, res) => {
    const chunks = []; for await (const c of req) chunks.push(c);
    const raw = Buffer.concat(chunks); let body; try {body = JSON.parse(raw);} catch {body = {};}
    captured.push({ path: req.url, method: req.method, body, raw: raw.toString(), headers: req.headers });
    // The model-relay probe: proves an allowed (loopback) host is still probed.
    if (req.url === '/v1/models') { res.setHeader('content-type','application/json'); res.end('{"data":[{"id":"fixture-model"}]}'); return; }
    if (req.url === '/api/llm/config/v2') {
      // A rejected official catalogue used to report only "Official catalogue unavailable".
      if (body.catalogue === 'fail') { res.statusCode=401; res.setHeader('content-type','application/json'); res.setHeader('retry-after','5'); res.end(JSON.stringify({error:{message:'invalid key synthetic-secret',type:'invalid_request_error',code:'invalid_api_key'}})); return; }
      res.setHeader('content-type','application/json'); res.end(JSON.stringify({success:true,code:'200',data:[{provider:'alibaba',modelList:[{modelCode:'official'}]}]})); return;
    }
    if (req.url === '/v1/chat/completions') {
      // A provider that rejects the key quotes the key it rejected; the gateway relays the reason
      // and not the credential, and keeps the status and the retry hint.
      if (body.model === 'limited') { res.statusCode=429; res.setHeader('content-type','application/json'); res.setHeader('retry-after','7'); res.setHeader('x-request-id','synthetic-request-id'); res.end(JSON.stringify({error:{message:`invalid key ${(req.headers.authorization||'').replace(/^Bearer /,'')}`,type:'invalid_request_error',code:'invalid_api_key'}})); return; }
      if (body.model === 'truncate') { res.setHeader('content-type','text/event-stream');res.end('data: {"choices":[{"delta":{"content":"partial"}}]}\n\n'); return; }
      if (body.model === 'tool') { res.setHeader('content-type','text/event-stream');res.end('data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"provider_call","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}\n\ndata: [DONE]\n\n'); return; }
      if (body.model === 'toolcutjson') { res.setHeader('content-type','application/json');res.end(JSON.stringify({choices:[{finish_reason:'length',message:{tool_calls:[{id:'cutj',function:{name:'read',arguments:'{"path":"a.tx'}}]}}],usage:{prompt_tokens:5,completion_tokens:9}})); return; }
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
  // A port nothing listens on, for the transport fallback: 502 must stay the connection failure.
  const deadPort=await new Promise(resolve=>{const probe=http.createServer();probe.listen(0,'127.0.0.1',()=>{const port=probe.address().port;probe.close(()=>resolve(port));});});
  const config={server:{host:'127.0.0.1',port:0},upstream:{baseUrl:upstream},catalog:{fetchOfficial:true},models:[
    {id:'openai',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'normal',apiKey:'synthetic-secret',enabled:true,aliases:['alias']},
    {id:'ollama',provider:'ollama',baseUrl:upstream,upstreamModel:'local',enabled:true},
    {id:'toolcut',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'toolcut',enabled:true},
    {id:'off',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'normal',enabled:false},
    {id:'toolusage',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'toolusage',enabled:true},
    {id:'toolcutjson',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'toolcutjson',enabled:true},
    {id:'tool',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'tool',enabled:true},
    {id:'truncate',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'truncate',enabled:true},
    {id:'limited',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'limited',apiKey:'synthetic-secret',enabled:true},
    {id:'unreachable',provider:'openai',baseUrl:`http://127.0.0.1:${deadPort}/v1`,upstreamModel:'normal',enabled:true},
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
  await check('a failing official catalogue reports what the upstream answered',async()=>{
    // A distinct body keeps this off the catalogue cache slot the previous check filled.
    const r=await req('/api/llm/config/v2','POST',{catalogue:'fail'});
    assert.equal(r.status,200);
    const v=await r.json();
    assert.strictEqual(v.success,true);
    assert.match(v.warning,/Official catalogue unavailable: official catalogue upstream returned HTTP 401/);
    assert.match(v.warning,/code=invalid_api_key/);
    assert.match(v.warning,/retry-after=5/);
    assert.ok(!v.warning.includes('synthetic-secret'),'the echoed key must be scrubbed');
    assert.equal(v.data[0].provider,'custom','the local models are still served');
  });
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
    for(const route of ['/','/index.html','/dashboard','/dashboard/','/dashboard/index.html']){
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
  await check('a non-stream length stop keeps its usage and refuses the call',async()=>{
    const r=await req('/api/adk/llm/generateContent','POST',{model:'toolcutjson',contents:[{parts:[{text:'x'}]}],stream:false});
    assert.strictEqual(r.status,200);
    const f=frames(await r.text());
    assert.ok(f.some(x=>x.error&&/length/.test(x.error.message)),JSON.stringify(f));
    assert.ok(!f.some(x=>x.finishReason==='TOOL_CALLS'),'a truncated call must not look runnable');
    assert.strictEqual(f.at(-1).usageMetadata.totalTokenCount,14,'the non-stream failure keeps its usage');
  });
  await check('a failed turn counts an error and is never counted as a success',async()=>{
    // The counters are what a caller reads to tell a working workflow from a broken one, so a turn
    // that failed must move `errors` and must not look like a completed success.
    const usage=async id=>(await(await req('/api/bridge/usage')).json()).usage[id]||{requests:0,errors:0};
    // The turn is counted when it starts and settled when it ends, so wait for the start to land.
    const settled=async(id,requests)=>{for(let i=0;i<50;i++){const value=await usage(id);if(value.requests===requests)return value;await new Promise(r=>setTimeout(r,20));}return await usage(id);};
    // A length-truncated tool call: the turn answers 200 with an error frame, and it is a failure.
    const cutBefore=await usage('toolcut');
    const cut=await generate('toolcut');
    assert.ok(cut.some(frame=>frame.error),JSON.stringify(cut));
    const cutAfter=await settled('toolcut',cutBefore.requests+1);
    assert.strictEqual(cutAfter.requests,cutBefore.requests+1,`the attempt is counted: ${JSON.stringify(cutAfter)}`);
    assert.strictEqual(cutAfter.errors,cutBefore.errors+1,`a truncated call is a failed turn: ${JSON.stringify(cutAfter)}`);
    // A mid-stream upstream interruption settles the same way.
    const truncatedBefore=await usage('truncate');
    const truncated=await generate('truncate');
    assert.ok(truncated.some(frame=>frame.error),JSON.stringify(truncated));
    const truncatedAfter=await settled('truncate',truncatedBefore.requests+1);
    assert.strictEqual(truncatedAfter.errors,truncatedBefore.errors+1,`an interrupted stream is a failed turn: ${JSON.stringify(truncatedAfter)}`);
    // The counters stay independent: a turn that succeeds moves requests only.
    const okBefore=await usage('openai');
    await generate('openai');
    const okAfter=await settled('openai',okBefore.requests+1);
    assert.strictEqual(okAfter.errors,okBefore.errors,`a successful turn is not an error: ${JSON.stringify(okAfter)}`);
  });
  await check('a colliding model name is refused on save, and the gateway keeps serving',async()=>{
    // 'openai' is an existing formal id; claiming it as another model's modelCode would leave two
    // models answering to one name, so the write must be refused while the editor is open.
    const r=await req('/api/bridge/models','POST',{id:'collide',provider:'openai',baseUrl:upstream+'/v1',modelCode:'openai'});
    assert.strictEqual(r.status,400,await r.text());
    assert.match((await (await fetch(base+'/api/bridge/models',{signal:AbortSignal.timeout(5000)})).json()).models.map(m=>m.id).join(','),/^((?!collide).)*$/);
    assert.strictEqual((await req('/healthz')).status,200);
  });
  await check('the panel is GET-only and answers 405 on every panel path',async()=>{
    // `POST /dashboard` used to reach the origin and come back 404 while `POST /` came back 405
    // from the origin: the same mistake got a different answer per path. The page is GET-only, so
    // every one of its paths refuses another method the same way.
    for(const route of ['/','/index.html','/dashboard','/dashboard/']){
      const post=await req(route,'POST',{});
      const body=await post.json();
      assert.strictEqual(post.status,405,`POST ${route}: ${JSON.stringify(body)}`);
      assert.strictEqual(body.status,405,`POST ${route}: ${JSON.stringify(body)}`);
      assert.strictEqual(body.error,'Method Not Allowed',`POST ${route}: ${JSON.stringify(body)}`);
      assert.ok(typeof body.timestamp==='string'&&typeof body.requestId==='string'&&typeof body.path==='string',JSON.stringify(body));
      assert.ok(!JSON.stringify(body).includes('Accio Gateway Bridge'),'a method the path does not accept must never be answered with the page');
      // GET is still the page, still served without a credential.
      const get=await req(route);
      assert.strictEqual(get.status,200,route);
      assert.match(get.headers.get('content-type'),/text\/html/,route);
    }
  });
  await check('an unknown management path is a 404 that does not claim the operation exists',async()=>{
    for(const [method,route] of [['GET','/api/bridge/nonexistent-probe'],['POST','/api/bridge/nonexistent-probe'],['GET','/__bridge/unknown']]){
      const response=await req(route,method,method==='POST'?{}:undefined);
      const text=await response.text();
      assert.strictEqual(response.status,404,`${method} ${route}: ${text}`);
      const body=JSON.parse(text);
      assert.strictEqual(body.status,404,text);
      assert.strictEqual(body.error,'Not Found',text);
      assert.strictEqual(body.path,route,text);
      assert.ok(typeof body.timestamp==='string'&&typeof body.requestId==='string',text);
      assert.ok(!('ok' in body),`a routing miss must not use the management answer shape: ${text}`);
      assert.ok(!text.includes('not implemented'),`a missing path must not claim the operation exists: ${text}`);
    }
  });
  // ---- the model relay is not a credential-relay primitive ------------------------------------
  await check('the model relay refuses a caller-supplied foreign host',async()=>{
    // `baseUrl` comes from the request body and the probe may carry a stored apiKey: an
    // unconstrained relay would aim this gateway's credential at an arbitrary host.
    const refused=await req('/api/bridge/discover/models','POST',{baseUrl:'http://example.com:9999/v1',apiKey:'relayed-secret'});
    const refusedText=await refused.text();
    assert.strictEqual(refused.status,403,refusedText);
    const body=JSON.parse(refusedText);
    assert.match(body.error,/discoveryAllowedHosts/);
    const text=JSON.stringify(body);
    assert.ok(!text.includes('relayed-secret'),'the refusal must not echo the key');
    assert.ok(!text.includes('example.com:9999/v1'),'the refusal must name the host, not the URL');
    // A loopback host (what the panel's own detection uses) is still probed.
    const allowed=await req('/api/bridge/discover/models','POST',{baseUrl:upstream+'/v1',apiKey:'relayed-secret'});
    assert.strictEqual(allowed.status,200);
    assert.deepStrictEqual((await allowed.json()).models,['fixture-model']);
  });
  // ---- one token, or a second secret: black-box on two extra gateways --------------------------
  const waitListening=kid=>new Promise((resolve,reject)=>{
    let out='',stderr='';
    const timeout=setTimeout(()=>reject(new Error('startup timeout '+stderr)),15000);
    kid.on('error',e=>{clearTimeout(timeout);reject(e);});
    kid.on('exit',code=>{clearTimeout(timeout);reject(new Error(`premature exit ${code}: ${stderr}`));});
    kid.stderr.on('data',c=>stderr+=c);
    kid.stdout.on('data',c=>{out+=c;let i;while((i=out.indexOf('\n'))>=0){const line=out.slice(0,i);out=out.slice(i+1);try{const msg=JSON.parse(line);if(msg.event==='listening'){clearTimeout(timeout);resolve(msg.port);}}catch{}}});
  });
  const spawnGateway=async(name,server,models=[{id:'openai',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'normal',apiKey:'synthetic-secret'}])=>{
    const configPath=path.join(dir,name);
    fs.writeFileSync(configPath,JSON.stringify({server:{host:'127.0.0.1',port:0,...server},upstream:{baseUrl:upstream},models}));
    const kid=spawn(binary,['--config',configPath,'--port','0'],{stdio:['ignore','pipe','pipe'],env:process.env});
    extraChildren.push(kid);
    return `http://127.0.0.1:${await waitListening(kid)}`;
  };
  const call=(base,route,headers={},method='GET',body)=>fetch(base+route,{method,headers:{...(body!==undefined?{'content-type':'application/json'}:{}),...headers},body:body===undefined?undefined:JSON.stringify(body),signal:AbortSignal.timeout(5000)});
  const splitBase=await spawnGateway('split-config.json',{token:'inference-secret',adminToken:'admin-secret',enforceToken:true});
  // A file that already contains the name collision still starts (adoption only warns, so the owner
  // can repair it in the panel), which is where the tier rule can be observed end to end: the write
  // path would refuse to save this configuration.
  const tierBase=await spawnGateway('tier-config.json',{},[
    {id:'off',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'normal',enabled:false},
    {id:'b2',provider:'openai',baseUrl:upstream+'/v1',upstreamModel:'shadowed',enabled:true,aliases:['off']},
  ]);
  await check('a tier whose hits are all disabled is refused instead of answered by a lower tier',async()=>{
    const before=captured.length;
    const disabled=await call(tierBase,'/api/adk/llm/generateContent',{},'POST',{model:'off',contents:[{parts:[{text:'hi'}]}]});
    const refused=await disabled.text();
    assert.strictEqual(disabled.status,404,refused);
    const body=JSON.parse(refused);
    assert.match(body.error,/configured but disabled/,refused);
    assert.ok(body.error.includes('off'),`the refusal has to name the model that was asked for: ${refused}`);
    assert.strictEqual(captured.length,before,'the request must not reach any provider');
    // The other model is still reachable by its own name, so nothing else about routing changed.
    const own=await call(tierBase,'/api/adk/llm/generateContent',{},'POST',{model:'b2',contents:[{parts:[{text:'hi'}]}]});
    const ownText=await own.text();
    assert.strictEqual(own.status,200,ownText);
    assert.strictEqual(captured.at(-1).body.model,'shadowed',`b2 answers with its own upstream model: ${ownText.slice(0,200)}`);
  });
  const singleBase=await spawnGateway('single-config.json',{token:'inference-secret',enforceToken:true});
  await check('an inference token is refused the management plane once an admin token exists',async()=>{
    const inference={'x-gateway-token':'inference-secret'};
    assert.strictEqual((await call(splitBase,'/api/bridge/status')).status,401,'a missing credential is a 401');
    const refused=await call(splitBase,'/api/bridge/status',inference);
    assert.strictEqual(refused.status,403,'an authenticated caller without the scope is a 403, not a 401');
    assert.match((await refused.json()).error,/management/);
    for(const route of ['/api/bridge/config','/api/bridge/models','/api/bridge/logs','/api/bridge/usage','/api/bridge/providers']){
      assert.strictEqual((await call(splitBase,route,inference)).status,403,route);
    }
    assert.strictEqual((await call(splitBase,'/api/bridge/accio/launch',inference,'POST','{}')).status,403,'the launch scope is not part of inference');
    assert.strictEqual((await call(splitBase,'/api/bridge/accio',inference)).status,403);
    assert.strictEqual((await call(splitBase,'/healthz',inference)).status,200,'the health check stays in the data plane');
    assert.strictEqual((await call(splitBase,'/dashboard')).status,200,'the panel page is still served without a credential');
  });
  await check('the admin token opens the management plane, and one token still opens everything',async()=>{
    const admin={'x-gateway-token':'admin-secret'};
    assert.strictEqual((await call(splitBase,'/api/bridge/status',admin)).status,200);
    assert.strictEqual((await call(splitBase,'/api/bridge/status',{'authorization':'Bearer admin-secret'})).status,200,'the admin secret is accepted as a bearer token too');
    // No admin token configured: today's single-token behaviour, unchanged.
    const single={'x-gateway-token':'inference-secret'};
    assert.strictEqual((await call(singleBase,'/api/bridge/status')).status,401);
    for(const route of ['/api/bridge/status','/api/bridge/config','/api/bridge/models','/api/bridge/providers','/api/bridge/usage','/api/bridge/logs','/api/bridge/logging']){
      assert.strictEqual((await call(singleBase,route,single)).status,200,route);
    }
    // The launch route is reached (an empty body is a 400) instead of being refused, so the single
    // token still holds that scope. No body means nothing is ever started.
    assert.strictEqual((await call(singleBase,'/api/bridge/accio/launch',single,'POST')).status,400);
  });
  await check('no response body echoes either secret',async()=>{
    for(const [base,headers] of [[splitBase,{'x-gateway-token':'admin-secret'}],[singleBase,{'x-gateway-token':'inference-secret'}]]){
      for(const route of ['/api/bridge/status','/api/bridge/config','/api/bridge/models','/api/bridge/providers','/api/bridge/usage','/api/bridge/logs','/api/bridge/logging','/dashboard']){
        const response=await call(base,route,headers);
        assert.strictEqual(response.status,200,route);
        const text=await response.text();
        for(const secret of ['admin-secret','inference-secret','synthetic-secret']){
          assert.ok(!text.includes(secret),`${route} echoed ${secret}`);
        }
      }
    }
    const config=await (await call(splitBase,'/api/bridge/config',{'x-gateway-token':'admin-secret'})).json();
    assert.strictEqual(config.config.server.hasAdminToken,true,'the panel may learn that a second secret exists');
    assert.strictEqual(config.config.server.token,undefined,'the token itself is never serialised');
  });
  await check('a bearer equal to a local credential is never relayed, an Accio session still is',async()=>{
    // Relaying the admin secret to the official upstream would hand it a local management
    // credential; Node parity requires an unrelated Accio session to keep travelling.
    captured.length=0;
    const admin=await call(splitBase,'/unknown',{'authorization':'Bearer admin-secret','x-gateway-token':'admin-secret'});
    assert.strictEqual(admin.status,207);
    await admin.text();
    assert.strictEqual(captured.at(-1).headers.authorization,undefined,'the admin bearer must not reach the upstream');
    const session=await call(splitBase,'/unknown',{'authorization':'Bearer accio-session','x-gateway-token':'admin-secret'});
    assert.strictEqual(session.status,207);
    await session.text();
    assert.strictEqual(captured.at(-1).headers.authorization,'Bearer accio-session');
  });
  await check('an upstream error keeps its status and message but never its credential',async()=>{
    const r=await req('/api/adk/llm/generateContent','POST',{model:'limited',contents:[{parts:[{text:'x'}]}]});
    // The upstream status is carried through: "upstream returned HTTP 429" alone hid both the
    // reason and whether a retry could help.
    assert.equal(r.status,429,'the upstream status replaces the flat 502');
    const body=await r.json();
    assert.match(body.error,/HTTP 429/);
    assert.match(body.error,/invalid key/);
    assert.match(body.error,/type=invalid_request_error/);
    assert.match(body.error,/code=invalid_api_key/);
    assert.match(body.error,/retry-after=7/);
    assert.ok(!JSON.stringify(body).includes('synthetic-secret'),'the echoed key must be scrubbed');
    const logs=(await(await req('/api/bridge/logs?lines=200')).json()).lines.map(line=>line.msg).join(' | ');
    assert.ok(logs.includes('status=429'),logs.slice(0,200));
    assert.ok(logs.includes('requestId=synthetic-request-id'),'the upstream request id must be logged');
    assert.ok(!logs.includes('synthetic-secret'),'the log must not carry the credential either');
    // 502 stays the transport fallback: no upstream answer was reached at all.
    const down=await req('/api/adk/llm/generateContent','POST',{model:'unreachable',contents:[{parts:[{text:'x'}]}]});
    assert.equal(down.status,502);
    assert.match((await down.json()).error,/connection failed/);
  });
  console.log(`${total-failures}/${total} black-box Rust core checks passed`);
  if (failures) process.exitCode = 1;
}
main().catch(e=>{console.error(e);process.exitCode=1;}).finally(async()=>{if(child && child.exitCode===null){child.kill('SIGTERM');await Promise.race([once(child,'exit'),new Promise(resolve=>setTimeout(resolve,2000))]);if(child.exitCode===null)child.kill('SIGKILL');}for(const extra of extraChildren){if(extra.exitCode===null)extra.kill('SIGKILL');}if(provider){provider.closeAllConnections();await new Promise(resolve=>provider.close(resolve));}fs.rmSync(dir,{recursive:true,force:true});});
