import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { JSDOM } from "jsdom";
const binary = fileURLToPath(
  new URL(
    `../../target/debug/accio-gateway-bridge-rust${process.platform === "win32" ? ".exe" : ""}`,
    import.meta.url,
  ),
);
test("UI through real local Rust core: connect/create/edit/disable/delete, token not persisted", async () => {
  const dir = await mkdtemp(path.join(tmpdir(), "accio-ui-live-"));
  const config = path.join(dir, "config.json");
  const child = spawn(
    binary,
    ["--config", config, "--host", "127.0.0.1", "--port", "0"],
    {
      env: { ...process.env, ACCIO_BRIDGE_TOKEN: "test-session-token" },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  try {
    const port = await new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error("startup timeout")),
        10000,
      );
      let output = "";
      child.once("error", reject);
      child.stdout.on("data", (d) => {
        output += d;
        for (const line of output.split("\n")) {
          try {
            const value = JSON.parse(line);
            if (value.event === "listening") {
              clearTimeout(timer);
              resolve(value.port);
            }
          } catch {}
        }
      });
    });
    const dom = new JSDOM(
      await readFile(new URL("../index.html", import.meta.url), "utf8"),
      { url: "http://localhost:3000" },
    );
    globalThis.window = dom.window;
    globalThis.document = dom.window.document;
    globalThis.FormData = dom.window.FormData;
    window.HTMLDialogElement.prototype.showModal = function () {
      this.open = true;
    };
    window.HTMLDialogElement.prototype.close = function () {
      this.open = false;
    };
    const $ = (id) => document.getElementById(id),
      wait = async (predicate) => {
        for (let i = 0; i < 200; i++) {
          if (predicate()) return;
          await new Promise((r) => setTimeout(r, 10));
        }
        throw new Error("UI state timeout");
      };
    await import("../app.js");
    $("port").value = port;
    $("token").value = "test-session-token";
    $("connect-form").dispatchEvent(
      new window.Event("submit", { cancelable: true }),
    );
    await wait(() => $("health").textContent === "运行中");
    assert.equal($("token").value, "");
    $("add").click();
    const form = $("model-form");
    for (const [k, v] of Object.entries({
      id: "offline-only",
      displayName: "离线测试",
      upstreamModel: "offline",
      baseUrl: "http://127.0.0.1:9",
      keyMode: "replace",
      apiKey: "fake-local-only",
    }))
      form.elements[k].value = v;
    form.dispatchEvent(new window.Event("submit", { cancelable: true }));
    await wait(() => $("count").textContent === "1");
    assert.match($("models").textContent, /离线测试/);
    assert.ok(!$("models").textContent.includes("fake-local-only"));
    $("models").querySelector("button").click();
    assert.equal(form.elements.apiKey.value, "");
    form.elements.displayName.value = "已编辑";
    form.dispatchEvent(new window.Event("submit", { cancelable: true }));
    await wait(() => $("models").textContent.includes("已编辑"));
    $("models").querySelectorAll("button")[3].click();
    await wait(() => $("models").textContent.includes("默认模型"));
    $("models").querySelectorAll("button")[1].click();
    await wait(() => $("active").textContent === "0");
    document.querySelector("[data-view=providers]").click();
    await wait(() => $("providers").querySelector("button"));
    $("providers").querySelector("button").click();
    const gf = $("group-form");
    gf.elements.baseUrl.value = "http://127.0.0.1:10";
    gf.dispatchEvent(new window.Event("submit", { cancelable: true }));
    assert.match($("group-error").textContent, /重新输入/);
    assert.equal($("group-editor").open, true);
    gf.elements.keyMode.value = "clear";
    gf.dispatchEvent(new window.Event("submit", { cancelable: true }));
    await wait(() => !$("group-editor").open);
    await wait(() => $("providers").textContent.includes("127.0.0.1:10"));
    assert.ok(!$("providers").textContent.includes("fake-local-only"));
    document.querySelector("[data-view=models]").click();
    $("models").querySelectorAll("button")[2].click();
    $("confirm-delete").click();
    await wait(() => $("count").textContent === "0");
    const disk = await readFile(config, "utf8");
    assert.ok(!disk.includes("test-session-token"));
    assert.ok(!disk.includes("fake-local-only"));
    $("disconnect").click();
    assert.equal($("health").textContent, "已断开");
    assert.equal(window.localStorage.length, 0);
    assert.equal(window.sessionStorage.length, 0);
  } finally {
    child.kill();
    await rm(dir, { recursive: true, force: true });
  }
});
