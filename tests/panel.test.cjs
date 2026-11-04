'use strict';
// DOM tests for the management panel that is embedded in the gateway binary:
// src/dashboard.html is read from disk and executed in jsdom, so the markup, the inline script,
// the element ids and the data-* contract exercised here are the shipped ones. It is not a copy
// of the panel, so it cannot drift away from it.
//
// Usage (from the repository root):  node tests/panel.test.cjs
// An alternative panel file may be passed as argv[2] (used to mutation-test the assertions).
// Requires tests/node_modules/jsdom (declared in tests/package.json):
//   npm install --prefix tests --registry=https://registry.npmmirror.com
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

let JSDOM;
try {
  ({ JSDOM } = require('jsdom'));
} catch (error) {
  // Never pass silently when the DOM environment is missing.
  console.error('FAIL cannot load jsdom, the panel cannot be exercised.');
  console.error('      install it once with: npm install --prefix tests --registry=https://registry.npmmirror.com');
  console.error(`      ${error.message}`);
  process.exit(1);
}

const PANEL = process.argv[2] ? path.resolve(process.argv[2]) : path.join(__dirname, '..', 'src', 'dashboard.html');
const PANEL_REL = process.argv[2] ? PANEL : 'src/dashboard.html';
const APOSTROPHE_ID = "O'Brien";
const INJECTION_ID = "x');window.__pwned=1;alert('pwned');//";
const IDS = [APOSTROPHE_ID, INJECTION_ID];
const TOKEN = 'panel-test-token-8f3c41';
// Any of these inside an inline handler (or in markup text) would mean the id was spliced into
// executable source instead of travelling as data.
const FRAGMENTS = [APOSTROPHE_ID, INJECTION_ID, '__pwned', 'pwned', "O'Brien", 'O&#39;Brien', 'alert('];
// Attributes that may legitimately carry a model id as their complete value: the two data-*
// hooks the row dispatcher reads, and the <option value> of the playground select.
const ID_ATTRIBUTES = ['data-model-id', 'data-model-toggle', 'value'];

const model = (id) => ({
  id,
  displayName: id,
  provider: 'openai',
  baseUrl: 'http://127.0.0.1:11434/v1',
  upstreamModel: 'demo',
  enabled: true,
  stream: true,
  contextWindow: 8192,
});

let total = 0;
let failures = 0;
async function check(name, fn) {
  total++;
  try {
    await fn();
    console.log(`PASS ${name}`);
  } catch (error) {
    failures++;
    console.error(`FAIL ${name}: ${error && error.stack ? error.stack : error}`);
  }
}

async function main() {
  const html = fs.readFileSync(PANEL, 'utf8');
  // Fail loudly if this is not the embedded panel (wrong path, or a stale copy somewhere).
  for (const needle of ['id="modelsTableBody"', 'id="tokenBadge"', 'id="tokenApplyBtn"', 'data-model-action', 'data-model-toggle']) {
    assert.ok(html.includes(needle), `${PANEL_REL} must be the embedded panel (missing ${needle})`);
  }

  const control = { status: 200, models: [] };
  const requests = [];
  const alerts = [];
  const consoleErrors = [];
  const dom = new JSDOM(html, {
    url: 'http://127.0.0.1:8080/dashboard',
    runScripts: 'dangerously',
    beforeParse(window) {
      window.alert = (message) => alerts.push(String(message));
      window.confirm = () => true;
      window.Element.prototype.scrollIntoView = function scrollIntoView() {};
      window.fetch = async (url, options) => {
        const opts = options || {};
        requests.push({
          url: String(url),
          method: String(opts.method || 'GET').toUpperCase(),
          headers: Object.assign({}, opts.headers || {}),
          body: opts.body === undefined ? undefined : String(opts.body),
        });
        const status = control.status;
        const payload = {
          models: control.models,
          server: { port: 8080, host: '127.0.0.1' },
          upstream: 'http://127.0.0.1:11434',
          accio: { installed: false, edition: 'unknown', running: false },
        };
        return {
          ok: status >= 200 && status < 300,
          status,
          json: async () => payload,
          text: async () => JSON.stringify(payload),
          headers: { get: () => null },
        };
      };
    },
  });

  const w = dom.window;
  const document = w.document;
  const $ = (id) => document.getElementById(id);
  const tick = () => new Promise((resolve) => setTimeout(resolve, 5));
  const waitFor = async (predicate, what) => {
    for (let i = 0; i < 400; i++) {
      if (predicate()) return;
      await tick();
    }
    throw new Error(`timeout waiting for ${what}`);
  };
  const elementsMatching = (attribute, value) =>
    Array.from(document.getElementsByTagName('*')).filter((el) => el.getAttribute(attribute) === value);
  const actionButton = (id, action) =>
    elementsMatching('data-model-id', id).find((el) => el.getAttribute('data-model-action') === action);
  const enableToggle = (id) => elementsMatching('data-model-toggle', id)[0];
  const tableBody = () => $('modelsTableBody');
  const refresh = async (models) => {
    if (models) control.models = models;
    const before = requests.length;
    $('refreshBtn').click();
    await waitFor(() => requests.length > before, 'a refresh request');
    await waitFor(
      () => (control.models.length === 0
        ? tableBody().textContent.includes('No custom models configured')
        : tableBody().querySelectorAll('tr').length === control.models.length),
      'the model table to be re-rendered',
    );
    await tick();
  };
  const inlineHandlers = () => {
    const found = [];
    for (const el of document.getElementsByTagName('*')) {
      for (const attribute of Array.from(el.attributes)) {
        if (/^on/i.test(attribute.name)) found.push(`${attribute.name}=${attribute.value}`);
      }
    }
    return found.sort();
  };
  const tokenHeader = (request) => {
    const key = Object.keys(request.headers).find((k) => k.toLowerCase() === 'x-gateway-token');
    return key === undefined ? undefined : request.headers[key];
  };
  const badgeVisible = () => $('tokenBadge').style.display !== 'none';
  const storageDump = (storage) =>
    Array.from({ length: storage.length }, (_, i) => `${storage.key(i)}=${storage.getItem(storage.key(i))}`);
  w.addEventListener('error', (event) => consoleErrors.push(String(event.message)));

  try {
    await waitFor(() => requests.length > 0, 'the panel to call the management API on load');
    await waitFor(() => tableBody().textContent.includes('No custom models configured'), 'the initial empty state');
    const baselineHandlers = inlineHandlers();

    await check('panel boots holding no token (empty input, hidden token bar, no header)', () => {
      assert.equal($('tokenInput').value, '');
      assert.equal(badgeVisible(), false);
      assert.equal(requests.every((r) => tokenHeader(r) === undefined), true);
      assert.equal($('modelsTableBody').querySelectorAll('[data-model-id]').length, 0);
    });

    // ---- 1. hostile model ids render inert and leave the row usable ----------------------
    await refresh([model(APOSTROPHE_ID), model(INJECTION_ID)]);

    await check(`apostrophe id ${JSON.stringify(APOSTROPHE_ID)} survives the attribute round-trip`, () => {
      const toggle = enableToggle(APOSTROPHE_ID);
      const play = actionButton(APOSTROPHE_ID, 'test');
      assert.ok(toggle && play, 'toggle + Play button must exist for the apostrophe id');
      assert.equal(toggle.getAttribute('data-model-toggle'), APOSTROPHE_ID);
      assert.equal(play.getAttribute('data-model-id'), APOSTROPHE_ID);
      assert.equal(actionButton(APOSTROPHE_ID, 'edit').getAttribute('data-model-id'), APOSTROPHE_ID);
      assert.equal(actionButton(APOSTROPHE_ID, 'delete').getAttribute('data-model-id'), APOSTROPHE_ID);
      const row = play.closest('tr');
      assert.equal(row.querySelectorAll('td').length, 7, 'the row must keep all 7 cells');
      assert.equal(row.querySelector('.model-id-code').textContent, APOSTROPHE_ID);
    });

    await check(`JS-significant id ${JSON.stringify(INJECTION_ID)} renders everywhere as data, never as code`, () => {
      const markup = tableBody().innerHTML;
      // The serialised rows must not contain a single inline handler: the row dispatcher reads
      // data-model-* instead, so nothing in the markup can interpret the id as JavaScript.
      assert.ok(!/\son[a-z]+\s*=/i.test(markup), 'generated rows must not contain inline event handlers');
      assert.ok(!/javascript:/i.test(markup), 'generated rows must not contain javascript: URLs');
      // Every occurrence of an id in the markup is accounted for either by an attribute value or
      // by a text node. A literal occurrence in any other position would be a string of code that
      // merely looks like markup, which is exactly the bug this panel was fixed for.
      const occ = (haystack, needle) => haystack.split(needle).length - 1;
      const textOccurrences = (id) => {
        let found = 0;
        const walker = document.createTreeWalker(tableBody(), w.NodeFilter.SHOW_TEXT);
        while (walker.nextNode()) found += occ(walker.currentNode.data, id);
        return found;
      };
      let markupCount = 0;
      let textCount = 0;
      let attributeCount = 0;
      for (const id of IDS) {
        markupCount += occ(markup, id);
        textCount += textOccurrences(id);
      }
      // Attribute values are matched textually: `onclick="f('x');...')"` would match here too and
      // is rejected by the allowed-name check below.
      const attributePattern = /([\w:-]+)\s*=\s*"([^"]*)"/g;
      for (let match = attributePattern.exec(markup); match; match = attributePattern.exec(markup)) {
        const [, name, value] = match;
        if (!FRAGMENTS.some((fragment) => value.includes(fragment))) continue;
        attributeCount += IDS.filter((id) => value.includes(id)).length;
        assert.ok(
          name.toLowerCase() === 'data-model-id' || name.toLowerCase() === 'data-model-toggle',
          `the id may only be a value of data-model-id / data-model-toggle, found ${name}="${value}"`,
        );
        assert.ok(IDS.includes(value), `${name} must hold the bare id, got ${JSON.stringify(value)}`);
      }
      assert.equal(attributeCount + textCount, markupCount, 'ids must appear only inside data-* attribute values or inert text');
      assert.ok(attributeCount >= 8, `every row action must carry the id (saw ${attributeCount})`);
      assert.ok(textCount > 0, 'the id is displayed as text');
      // The document-wide scan: no attribute outside the data hooks / playground option value
      // contains any part of an id, and no attribute value mixes an id with other source text.
      const holders = [];
      for (const el of document.getElementsByTagName('*')) {
        for (const attribute of Array.from(el.attributes)) {
          if (!FRAGMENTS.some((fragment) => attribute.value.includes(fragment))) continue;
          holders.push(`${attribute.name}=${JSON.stringify(attribute.value)}`);
          assert.ok(
            ID_ATTRIBUTES.includes(attribute.name),
            `id leaked into attribute ${attribute.name}=${JSON.stringify(attribute.value)}`,
          );
          assert.ok(IDS.includes(attribute.value), `${attribute.name} must hold the bare id, got ${JSON.stringify(attribute.value)}`);
          assert.ok(!/^on/i.test(attribute.name), `${attribute.name} is an inline handler built from the id`);
        }
      }
      assert.ok(holders.length >= 8, `expected the ids to be found in data attributes, saw ${holders.length}`);
      // the id is also displayed as inert text: it creates no elements
      const cell = actionButton(INJECTION_ID, 'test').closest('tr').querySelector('.model-id-code');
      assert.equal(cell.textContent, INJECTION_ID);
      assert.equal(cell.children.length, 0);
      // nothing executed while rendering, and the row is intact
      assert.equal(w.__pwned, undefined);
      assert.deepEqual(alerts, []);
      assert.equal(actionButton(INJECTION_ID, 'test').closest('tr').querySelectorAll('td').length, 7);
      assert.equal(tableBody().querySelectorAll('script, img, iframe').length, 0);
    });

    await check('no onclick/onchange anywhere mentions either id, and rendering adds no handler', () => {
      const handlers = inlineHandlers();
      assert.deepEqual(handlers, baselineHandlers, 'the panel must not synthesise inline handlers from model ids');
      for (const handler of handlers) {
        for (const fragment of FRAGMENTS) {
          assert.ok(!handler.includes(fragment), `inline handler contains ${JSON.stringify(fragment)}: ${handler}`);
        }
      }
    });

    await check('clicking the real Play button forwards the exact id and executes nothing', async () => {
      const before = alerts.length;
      actionButton(INJECTION_ID, 'test').click();
      await tick();
      assert.equal($('playModelSelect').value, INJECTION_ID, 'the playground select must receive the exact id');
      actionButton(APOSTROPHE_ID, 'test').click();
      await tick();
      assert.equal($('playModelSelect').value, APOSTROPHE_ID);
      assert.equal(w.__pwned, undefined);
      assert.equal(alerts.length, before, 'no alert may fire');
      assert.equal(typeof w.alert, 'function');
    });

    // ---- 2. the delegated dispatcher forwards the exact id -------------------------------
    const dispatched = [];
    for (const name of ['selectModelForTest', 'openEditModelModal', 'deleteModel', 'toggleModelEnabled']) {
      assert.equal(typeof w[name], 'function', `${name} must exist as a panel global (spy target)`);
      w[name] = (...args) => dispatched.push([name, ...args]);
    }

    await check('Play/Edit/Delete call selectModelForTest/openEditModelModal/deleteModel with the exact id', async () => {
      for (const id of IDS) {
        for (const [action, fn] of [['test', 'selectModelForTest'], ['edit', 'openEditModelModal'], ['delete', 'deleteModel']]) {
          dispatched.length = 0;
          actionButton(id, action).click();
          await tick();
          assert.deepEqual(dispatched, [[fn, id]], `${action} on ${JSON.stringify(id)}`);
        }
      }
      assert.equal(w.__pwned, undefined);
      assert.deepEqual(alerts, []);
    });

    await check('the enable checkbox calls toggleModelEnabled(exact id, new checked state)', async () => {
      for (const id of IDS) {
        for (const next of [false, true]) {
          const toggle = enableToggle(id);
          toggle.checked = next;
          dispatched.length = 0;
          toggle.dispatchEvent(new w.Event('change', { bubbles: true }));
          await tick();
          assert.deepEqual(dispatched, [['toggleModelEnabled', id, next]], `toggle -> ${next} on ${JSON.stringify(id)}`);
        }
      }
      assert.equal(w.__pwned, undefined);
    });

    // ---- 3. token flow -------------------------------------------------------------------
    await check('without a token no management request carries an x-gateway-token header', async () => {
      const before = requests.length;
      $('refreshBtn').click();
      await waitFor(() => requests.length > before, 'a refresh request');
      assert.ok(requests.length >= 2, 'several management requests must have been made by now');
      for (const request of requests) {
        assert.equal(tokenHeader(request), undefined, `${request.method} ${request.url} must not carry a token`);
      }
      assert.equal(badgeVisible(), false);
    });

    await check('Connect sends exactly the typed token on later requests; a 200 keeps the bar hidden', async () => {
      control.status = 200;
      $('tokenInput').value = TOKEN;
      const before = requests.length;
      $('tokenApplyBtn').click();
      await waitFor(() => requests.length > before, 'the post-Connect refresh');
      await waitFor(() => $('bridgeStatusText').innerText === 'Bridge Running', 'the 200 response to be processed');
      for (const request of requests.slice(before)) {
        assert.equal(tokenHeader(request), TOKEN, `${request.method} ${request.url}`);
      }
      assert.equal($('tokenInput').value, TOKEN, 'the typed token stays in the input');
      assert.equal(badgeVisible(), false, 'a 200 response must not show the token bar');

      const beforeRefresh = requests.length;
      $('refreshBtn').click();
      await waitFor(() => requests.length > beforeRefresh, 'another refresh request');
      for (const request of requests.slice(beforeRefresh)) {
        assert.equal(tokenHeader(request), TOKEN, `${request.method} ${request.url}`);
      }
    });

    await check('a 401 shows the token bar; reconnecting on a 200 hides it again', async () => {
      control.status = 401;
      const before = requests.length;
      $('refreshBtn').click();
      await waitFor(() => requests.length > before, 'the 401 request');
      await waitFor(() => badgeVisible(), 'the token bar to become visible');
      assert.equal(tokenHeader(requests[requests.length - 1]), TOKEN, 'the rejected request still carried the token');

      control.status = 200;
      const beforeApply = requests.length;
      $('tokenApplyBtn').click();
      await waitFor(() => requests.length > beforeApply, 'the reconnect request');
      await waitFor(() => $('bridgeStatusText').innerText === 'Bridge Running', 'the recovered status');
      assert.equal(badgeVisible(), false, 'a 200 response must not show the token bar');
      assert.equal(tokenHeader(requests[requests.length - 1]), TOKEN);
    });

    // ---- 4. the token never leaves memory ------------------------------------------------
    await check('the token appears in no URL, cookie, localStorage or sessionStorage entry', () => {
      const haystacks = [
        ['location.href', w.location.href],
        ['location.search', w.location.search],
        ['location.hash', w.location.hash],
        ['document.cookie', document.cookie],
        ['localStorage', storageDump(w.localStorage).join('&')],
        ['sessionStorage', storageDump(w.sessionStorage).join('&')],
        ['document.documentElement.outerHTML', document.documentElement.outerHTML],
      ];
      for (const [where, value] of haystacks) {
        assert.ok(!String(value).includes(TOKEN), `token leaked into ${where}`);
      }
      for (const request of requests) {
        assert.ok(!request.url.includes(TOKEN), `token leaked into the URL of ${request.url}`);
        assert.ok(!String(request.body || '').includes(TOKEN), `token leaked into a request body`);
      }
      for (const [where, storage] of [['localStorage', w.localStorage], ['sessionStorage', w.sessionStorage]]) {
        for (const entry of storageDump(storage)) {
          assert.ok(!entry.includes(TOKEN), `token leaked into ${where}: ${entry}`);
          assert.ok(!/token|secret|auth/i.test(entry), `${where} must not persist credential-shaped data: ${entry}`);
        }
      }
      assert.deepEqual(consoleErrors, [], 'the panel must not raise script errors');
    });
  } finally {
    dom.window.close();
  }

  const covered = [
    'id escaping + inert rows (no inline on*, no execution)',
    'Play/Edit/Delete/toggle dispatch the exact id',
    'no token header before Connect, exact token after',
    '401 shows tokenBadge, 200 keeps it hidden',
    'token absent from URL/cookie/localStorage/sessionStorage',
  ].join('; ');
  console.log(
    `${total - failures}/${total} embedded panel checks passed | ${PANEL_REL} (real file, jsdom ${require('jsdom/package.json').version}) | ids ${JSON.stringify(APOSTROPHE_ID)} + ${JSON.stringify(INJECTION_ID)} | ${covered}`,
  );
  if (failures) process.exitCode = 1;
}

main()
  .catch((error) => {
    console.error(error && error.stack ? error.stack : error);
    process.exitCode = 1;
  })
  .finally(() => {
    // jsdom keeps the panel's setInterval(loadStatus, 10000) alive; never let it hang the runner.
    setTimeout(() => process.exit(process.exitCode || 0), 25).unref();
  });
