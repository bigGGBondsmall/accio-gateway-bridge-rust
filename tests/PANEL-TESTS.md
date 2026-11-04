# Embedded panel DOM tests

`panel.test.cjs` drives the management panel that is embedded in the gateway binary
(`src/dashboard.html`, `include_str!`-ed and served on `/`, `/index.html`, `/dashboard`,
`/dashboard/`). The real file is read from disk and executed in jsdom, so the test always
exercises the shipped markup and inline script — it is not a copy and cannot drift.

## Run it (from the repository root)

```sh
node tests/panel.test.cjs
```

Exit code is `0` only when every check passes; it prints one line per check plus a one-line
summary. Nothing is fetched from the network: all management API calls are answered by an
in-test `fetch` double.

## One-time setup (jsdom)

The DOM environment is jsdom, the same library the Tauri UI tests in `ui/` use. It is installed
locally under `tests/node_modules` (ignored by git):

```sh
npm install --prefix tests --registry=https://registry.npmmirror.com
```

If jsdom is missing the test fails loudly (exit code `1`) instead of passing silently.

`npm test` inside `tests/` is equivalent, and an alternative panel file can be passed as the
first argument (used to mutation-test the assertions):

```sh
node tests/panel.test.cjs path/to/other-dashboard.html
```

## What it checks

1. Model ids that used to be spliced into inline handlers — `O'Brien` and
   `x');window.__pwned=1;alert('pwned');//` — render safely: each id appears only as the whole
   value of a `data-model-id` / `data-model-toggle` attribute (or as inert text), the generated
   rows contain no `onclick`/`onchange` attribute at all, no attribute anywhere in the document
   mixes an id into other source text, and `window.__pwned` stays undefined.
2. Play / Edit / Delete dispatch the exact id string to `selectModelForTest` /
   `openEditModelModal` / `deleteModel`, and the enable checkbox calls
   `toggleModelEnabled(id, newChecked)`.
3. Token flow: no `x-gateway-token` header before a token is entered; after typing into
   `#tokenInput` and clicking `#tokenApplyBtn` every later management request carries exactly
   that token; a `401` makes `#tokenBadge` visible and a `200` leaves it hidden.
4. The token never appears in a request URL, `document.cookie`, `localStorage` or
   `sessionStorage`.
