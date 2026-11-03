import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { JSDOM } from "jsdom";
import { buildModel, safeError } from "../model.js";
const html = readFileSync(new URL("../index.html", import.meta.url), "utf8");
const dom = new JSDOM(html, { url: "http://localhost:3000" });
globalThis.window = dom.window;
globalThis.document = dom.window.document;
globalThis.FormData = dom.window.FormData;
dom.window.HTMLDialogElement.prototype.showModal = function () {
  this.open = true;
};
dom.window.HTMLDialogElement.prototype.close = function () {
  this.open = false;
};
const $ = (id) => document.getElementById(id);
const model = {
  id: "demo",
  displayName: "Demo",
  provider: "openai",
  baseUrl: "https://example.test/v1",
  upstreamModel: "demo",
  enabled: true,
  stream: true,
  aliases: [],
  hasKey: true,
};
let records = [model],
  calls = [];
window.__TAURI__ = {
  core: {
    invoke: async (command, { path, method, body }) => {
      calls.push({ command, path, method, body });
      if (path === "/api/bridge/status")
        return {
          ok: true,
          models: records,
          modelsCount: records.length,
          activeModelsCount: records.filter((m) => m.enabled).length,
        };
      if (path === "/api/bridge/models" && method === "POST") {
        records = [{ ...body, hasKey: true }];
        return { ok: true };
      }
      if (path.startsWith("/api/bridge/models?id=") && method === "DELETE") {
        records = [];
        return { ok: true };
      }
      if (path === "/api/bridge/discover")
        return {
          ok: true,
          services: [
            {
              name: "Fixture service",
              baseUrl: "http://127.0.0.1:11434",
              provider: "ollama",
              models: ["fixture-only"],
            },
          ],
        };
      if (path === "/api/bridge/accio")
        return {
          ok: true,
          accio: { installed: true, edition: "unknown", running: false },
        };
      if (path === "/api/bridge/accio/launch")
        return { ok: true, launched: true };
      if (path === "/api/bridge/config")
        return {
          config: {
            catalog: {
              enabled: true,
              fetchOfficial: false,
              providerDisplayName: "Custom",
              timeoutMs: 8000,
            },
            upstream: { timeoutMs: 300000 },
          },
        };
      return { ok: true };
    },
  },
};
const tick = () => new Promise((r) => setTimeout(r, 0));
await import("../app.js");
await tick();
test("initial connected native status and token field hidden", () => {
  assert.equal($("health").textContent, "运行中");
  assert.equal($("count").textContent, "1");
  assert.equal($("connection").hidden, true);
  assert.equal($("token").value, "");
});
test("edit never renders saved API key; cancel clears input", () => {
  $("models").querySelector("button").click();
  assert.equal($("editor").open, true);
  assert.equal($("model-form").elements.apiKey.value, "");
  $("model-form").elements.apiKey.value = "temp-secret";
  $("cancel-editor").click();
  assert.equal($("editor").open, false);
  assert.equal($("model-form").elements.apiKey.value, "");
});
test("changing destination requires explicit credential choice", () => {
  $("models").querySelector("button").click();
  const f = $("model-form");
  f.elements.baseUrl.value = "https://other.test/v1";
  assert.throws(() => buildModel(f, model), /重新输入密钥/);
  f.elements.keyMode.value = "clear";
  assert.equal(buildModel(f, model).apiKey, "");
  f.elements.keyMode.value = "replace";
  f.elements.keyMode.dispatchEvent(new window.Event("change"));
  f.elements.apiKey.value = "new-key";
  assert.equal(buildModel(f, model).apiKey, "new-key");
  $("cancel-editor").click();
});
test("unchanged edit omits secret; new clear is explicit", () => {
  $("models").querySelector("button").click();
  assert.equal(
    Object.hasOwn(buildModel($("model-form"), model), "apiKey"),
    false,
  );
  $("cancel-editor").click();
  $("add").click();
  const f = $("model-form");
  f.elements.id.value = "a";
  f.elements.upstreamModel.value = "a";
  f.elements.baseUrl.value = "http://localhost:11434";
  f.elements.keyMode.value = "clear";
  assert.equal(buildModel(f).apiKey, "");
  $("cancel-editor").click();
});
test("unsafe credential URL rejected", () => {
  $("models").querySelector("button").click();
  const f = $("model-form");
  f.elements.baseUrl.value = "https://user:password@example.test";
  assert.throws(() => buildModel(f, model), /用户名/);
  $("cancel-editor").click();
});
test("search produces real empty state", () => {
  $("search").value = "nothing";
  $("search").dispatchEvent(new window.Event("input"));
  assert.match($("models").textContent, /没有匹配/);
  $("search").value = "";
  $("search").dispatchEvent(new window.Event("input"));
});
test("disable uses real POST and no key leakage", async () => {
  $("models").querySelectorAll("button")[1].click();
  await tick();
  assert.equal(records[0].enabled, false);
  assert.equal(
    Object.hasOwn(calls.find((c) => c.method === "POST").body, "apiKey"),
    false,
  );
});
test("delete cancel makes no request, confirmation deletes", async () => {
  const before = calls.length;
  $("models").querySelectorAll("button")[2].click();
  $("cancel-delete").click();
  assert.equal(calls.length, before);
  $("models").querySelectorAll("button")[2].click();
  $("confirm-delete").click();
  await tick();
  assert.equal(records.length, 0);
  assert.match($("models").textContent, /还没有配置/);
});
test("diagnostics never calls model endpoints", async () => {
  const before = calls.length;
  $("diagnose").click();
  await tick();
  const requests = calls.slice(before);
  assert.deepEqual(
    requests.map((c) => c.path),
    ["/healthz", "/api/bridge/status", "/api/bridge/models"],
  );
  assert.equal($("checks").children.length, 3);
});
test("error surface never echoes credentials", () => {
  assert.ok(!safeError(new Error("api-key-secret")).includes("api-key-secret"));
});
test("untrusted model labels inserted as text", async () => {
  records = [{ ...model, displayName: "<img src=x onerror=alert(1)>" }];
  $("refresh").click();
  await tick();
  assert.equal($("models").querySelector("img"), null);
  assert.match($("models").textContent, /<img/);
});

test("settings save uses config endpoint, not model request", async () => {
  document.querySelector("[data-view=settings]").click();
  await tick();
  $("settings-form").elements["catalog.providerDisplayName"].value = "本机目录";
  $("settings-form").dispatchEvent(
    new window.Event("submit", { cancelable: true }),
  );
  await tick();
  const request = calls.findLast((c) => c.method === "PUT");
  assert.equal(request.path, "/api/bridge/config");
  assert.equal(request.body.catalog.providerDisplayName, "本机目录");
});
test("local discovery is explicit and selecting result only prefills editor", async () => {
  const before = calls.filter((c) => c.path === "/api/bridge/discover").length;
  assert.equal(before, 0);
  $("discover").click();
  await tick();
  assert.match($("discovered").textContent, /Fixture service/);
  const writes = calls.filter((c) => c.method !== "GET").length;
  $("discovered").querySelector("button").click();
  assert.equal($("model-form").elements.provider.value, "ollama");
  assert.equal($("model-form").elements.upstreamModel.value, "fixture-only");
  assert.equal(calls.filter((c) => c.method !== "GET").length, writes);
  $("cancel-editor").click();
});
test("Accio launch requires separate confirmation, cancel never launches (mock only)", async () => {
  $("inspect-accio").click();
  await tick();
  assert.equal($("launch-accio").disabled, false);
  $("launch-accio").click();
  assert.equal($("launch-dialog").open, true);
  $("cancel-launch").click();
  assert.equal(
    calls.filter((c) => c.path === "/api/bridge/accio/launch").length,
    0,
  );
  $("launch-accio").click();
  $("force-launch").checked = true;
  $("confirm-launch").click();
  await tick();
  const requests = calls.filter((c) => c.path === "/api/bridge/accio/launch");
  assert.equal(requests.length, 1);
  assert.deepEqual(requests[0].body, { force: true });
  assert.equal($("launch-dialog").open, false);
});
test("management outage clears stale success indicators and raw error is never rendered", async () => {
  const old = window.__TAURI__.core.invoke;
  window.__TAURI__.core.invoke = async () => {
    throw new Error("secret-from-upstream");
  };
  $("refresh").click();
  await tick();
  assert.equal($("health").textContent, "未连接");
  assert.equal($("count").textContent, "—");
  assert.ok(!document.body.textContent.includes("secret-from-upstream"));
  window.__TAURI__.core.invoke = old;
});
