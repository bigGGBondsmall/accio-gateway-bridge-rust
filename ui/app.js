import { buildModel, safeError } from "./model.js";
const $ = (id) => document.getElementById(id);
const native = !!window.__TAURI__?.core?.invoke;
let token = "",
  base = "",
  models = [],
  original = null,
  deleting = null,
  connected = native,
  epoch = 0,
  busy = false;
function notice(message) {
  $("notice").textContent = message;
  $("notice").hidden = !message;
}
async function api(path, method = "GET", body) {
  if (native)
    return window.__TAURI__.core.invoke("management_request", {
      path,
      method,
      body: body ?? null,
    });
  if (!connected) throw new Error("disconnected");
  const response = await fetch(base + path, {
    method,
    headers: {
      "X-Gateway-Token": token,
      ...(body ? { "Content-Type": "application/json" } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
    signal: AbortSignal.timeout(12000),
    credentials: "omit",
    redirect: "error",
  });
  if (!response.ok) throw new Error("request failed");
  return response.json();
}
function element(tag, text, cls) {
  const e = document.createElement(tag);
  if (text !== undefined) e.textContent = text;
  if (cls) e.className = cls;
  return e;
}
function action(text, callback, cls = "secondary") {
  const b = element("button", text, cls);
  b.type = "button";
  b.onclick = callback;
  return b;
}
function render() {
  const root = $("models");
  root.replaceChildren();
  const query = $("search").value.toLowerCase();
  const filtered = models.filter((m) =>
    [m.id, m.displayName, m.provider, m.upstreamModel].some((v) =>
      String(v ?? "")
        .toLowerCase()
        .includes(query),
    ),
  );
  $("result-count").textContent = `${filtered.length} 个模型`;
  if (!filtered.length) {
    root.append(
      element(
        "div",
        models.length
          ? "没有匹配的模型。试试其他关键词。"
          : "还没有配置模型。添加你的第一个模型，开始连接。",
        "empty",
      ),
    );
    return;
  }
  for (const m of filtered) {
    const card = element("article", undefined, "model-card"),
      top = element("div", undefined, "card-top"),
      names = element("div");
    names.append(element("h3", m.displayName || m.id), element("code", m.id));
    top.append(
      names,
      element(
        "span",
        m.enabled ? "已启用" : "已停用",
        `badge ${m.enabled ? "" : "off"}`,
      ),
    );
    const meta = element("div", undefined, "metadata");
    meta.append(
      element("span", `${m.provider} · ${m.upstreamModel}`),
      element("span", m.baseUrl),
      element(
        "span",
        `${m.hasKey ? "● 已保存密钥" : "○ 未设置密钥"}　·　${m.stream ? "流式" : "非流式"}`,
      ),
    );
    const actions = element("div", undefined, "card-actions");
    actions.append(
      action("编辑", () => edit(m)),
      action(m.enabled ? "停用" : "启用", () => toggle(m)),
      action("删除", () => {
        if (busy) return;
        deleting = m;
        $("delete-name").textContent = m.displayName || m.id;
        $("delete-dialog").showModal();
      }),
    );
    const defaultButton = action(
      m.isDefault ? "默认模型" : "设为默认",
      async () => {
        if (busy || m.isDefault) return;
        busy = true;
        try {
          await api(`/api/bridge/models/${encodeURIComponent(m.id)}`, "PATCH", {
            isDefault: true,
          });
          await refresh();
        } catch (e) {
          notice(safeError(e));
        } finally {
          busy = false;
        }
      },
    );
    defaultButton.disabled = !!m.isDefault;
    actions.append(defaultButton);
    card.append(top, meta, actions);
    root.append(card);
  }
}
async function refresh() {
  const current = ++epoch;
  $("refresh").disabled = true;
  try {
    const s = await api("/api/bridge/status");
    if (current !== epoch) return;
    models = s.models || [];
    $("health").textContent = "运行中";
    $("count").textContent = String(s.modelsCount ?? models.length);
    $("active").textContent = String(
      s.activeModelsCount ?? models.filter((m) => m.enabled).length,
    );
    $("version").textContent = `v${s.version || "0.1.0"}`;
    $("address").textContent = native
      ? `http://127.0.0.1:${s.server?.port ?? "—"} · 本地托管`
      : base;
    render();
    notice("");
    if (!$("providers-view").hidden) await loadProviders();
  } catch (e) {
    if (current !== epoch) return;
    $("health").textContent = "未连接";
    $("count").textContent = "—";
    $("active").textContent = "—";
    models = [];
    render();
    notice(safeError(e));
  } finally {
    if (current === epoch) $("refresh").disabled = false;
  }
}
function edit(m = null) {
  if (busy) return;
  original = m;
  $("model-form").reset();
  $("editor-error").hidden = true;
  $("editor-title").textContent = m ? "编辑模型" : "添加模型";
  const f = $("model-form").elements;
  for (const key of [
    "id",
    "displayName",
    "provider",
    "upstreamModel",
    "baseUrl",
  ])
    f[key].value = m?.[key] ?? (key === "provider" ? "openai" : "");
  f.id.readOnly = !!m;
  f.aliases.value = (m?.aliases || []).join(", ");
  f.enabled.checked = m?.enabled ?? true;
  f.stream.checked = m?.stream ?? true;
  f.keyMode.value = m ? "keep" : "replace";
  f.apiKey.disabled = !!m;
  f.apiKey.value = "";
  $("key-hint").textContent = m?.hasKey
    ? "已有密钥。留空保留；变更地址或协议时必须重新确认密钥。"
    : "没有保存的密钥。无需密钥的接口请选择「明确清空密钥」。";
  $("editor").showModal();
}
function closeEditor() {
  if (busy) return;
  $("editor").close();
  $("model-form").reset();
  $("model-form").elements.apiKey.value = "";
  original = null;
}
$("editor").addEventListener("cancel", (e) => {
  e.preventDefault();
  closeEditor();
});
$("close-editor").onclick = closeEditor;
$("cancel-editor").onclick = closeEditor;
$("add").onclick = () => edit();
$("search").oninput = render;
$("refresh").onclick = refresh;
$("model-form").elements.keyMode.onchange = () => {
  const f = $("model-form").elements;
  f.apiKey.value = "";
  f.apiKey.disabled = f.keyMode.value !== "replace";
};
$("model-form").onsubmit = async (e) => {
  e.preventDefault();
  if (busy) return;
  let model;
  try {
    model = buildModel(e.target, original);
  } catch (err) {
    $("editor-error").textContent = err.message;
    $("editor-error").hidden = false;
    return;
  }
  busy = true;
  $("save-model").disabled = true;
  try {
    await api("/api/bridge/models", "POST", model);
    busy = false;
    closeEditor();
    await refresh();
    notice("模型已保存。");
  } catch (err) {
    $("editor-error").textContent = safeError(err);
    $("editor-error").hidden = false;
  } finally {
    busy = false;
    $("save-model").disabled = false;
  }
};
async function toggle(m) {
  if (busy) return;
  busy = true;
  try {
    const update = { ...m, enabled: !m.enabled };
    delete update.hasKey;
    await api("/api/bridge/models", "POST", update);
    await refresh();
  } catch (e) {
    notice(safeError(e));
  } finally {
    busy = false;
  }
}
$("cancel-delete").onclick = () => {
  if (!busy) $("delete-dialog").close();
};
$("delete-dialog").addEventListener("cancel", (e) => {
  if (busy) e.preventDefault();
});
$("confirm-delete").onclick = async () => {
  if (busy || !deleting) return;
  busy = true;
  $("confirm-delete").disabled = true;
  try {
    await api(
      `/api/bridge/models?id=${encodeURIComponent(deleting.id)}`,
      "DELETE",
    );
    $("delete-dialog").close();
    deleting = null;
    await refresh();
    notice("模型已删除。");
  } catch (e) {
    notice(safeError(e));
  } finally {
    busy = false;
    $("confirm-delete").disabled = false;
  }
};
const settingFields = [
  ["catalog", "enabled", "启用自定义模型目录", "checkbox"],
  [
    "catalog",
    "fetchOfficial",
    "合并官方模型目录（Accio 查询时可能请求官方接口）",
    "checkbox",
  ],
  ["catalog", "providerDisplayName", "目录服务商显示名称", "text"],
  ["catalog", "timeoutMs", "目录请求超时（毫秒）", "number"],
  ["upstream", "timeoutMs", "上游请求超时（毫秒）", "number"],
];
let configLoaded = false;
async function loadSettings() {
  configLoaded = false;
  $("save-settings").disabled = true;
  try {
    const { config } = await api("/api/bridge/config");
    const container = $("settings-fields");
    container.replaceChildren();
    for (const [section, key, label, type] of settingFields) {
      const l = element("label", label, type === "checkbox" ? "check" : "");
      const i = element("input");
      i.type = type;
      i.name = `${section}.${key}`;
      if (type === "checkbox") i.checked = !!config[section][key];
      else i.value = config[section][key];
      if (type === "number") {
        i.min = "100";
        i.max = "3600000";
        i.required = true;
      }
      l.append(i);
      container.append(l);
    }
    configLoaded = true;
    $("save-settings").disabled = false;
  } catch (e) {
    notice(safeError(e));
  }
}
$("settings-form").onsubmit = async (e) => {
  e.preventDefault();
  if (!configLoaded || busy) return;
  busy = true;
  $("save-settings").disabled = true;
  const body = {};
  for (const [section, key, , type] of settingFields) {
    const input = e.target.elements[`${section}.${key}`];
    (body[section] ??= {})[key] =
      type === "checkbox"
        ? input.checked
        : type === "number"
          ? Number(input.value)
          : input.value;
  }
  try {
    await api("/api/bridge/config", "PUT", body);
    notice("设置已保存。");
  } catch (err) {
    notice(safeError(err));
  } finally {
    busy = false;
    $("save-settings").disabled = false;
  }
};
for (const b of document.querySelectorAll("nav button"))
  b.onclick = () => {
    for (const n of document.querySelectorAll("nav button"))
      n.classList.toggle("selected", n === b);
    for (const v of ["models", "providers", "settings", "diagnostics"])
      $(`${v}-view`).hidden = v !== b.dataset.view;
    $("title").textContent = b.textContent.trim().slice(1).trim();
    if (b.dataset.view === "settings") loadSettings();
    if (b.dataset.view === "providers") loadProviders();
  };
$("diagnose").onclick = async () => {
  const button = $("diagnose");
  button.disabled = true;
  $("checks").replaceChildren();
  for (const [path, label] of [
    ["/healthz", "本地服务响应"],
    ["/api/bridge/status", "管理鉴权"],
    ["/api/bridge/models", "模型配置读取"],
  ]) {
    try {
      await api(path);
      $("checks").append(element("li", `✓ ${label}正常`));
    } catch {
      $("checks").append(element("li", `✕ ${label}失败：请检查连接与权限`));
    }
  }
  button.disabled = false;
};
$("connection").hidden = native;
$("connect-form").onsubmit = (e) => {
  e.preventDefault();
  token = $("token").value;
  $("token").value = "";
  base = `http://127.0.0.1:${Number($("port").value)}`;
  connected = true;
  refresh();
};
$("disconnect").onclick = () => {
  ++epoch;
  connected = false;
  token = "";
  base = "";
  $("token").value = "";
  models = [];
  $("health").textContent = "已断开";
  $("count").textContent = "—";
  $("active").textContent = "—";
  $("address").textContent = "等待连接";
  $("refresh").disabled = false;
  render();
  notice("管理令牌已从当前页面清除。");
};
if (native) refresh();
else {
  $("health").textContent = "未连接";
  render();
}

let currentGroup = null;
async function loadProviders() {
  $("providers").replaceChildren(element("div", "正在读取服务商…", "empty"));
  try {
    const result = await api("/api/bridge/providers");
    const groups = result.providers || [];
    $("providers").replaceChildren();
    if (!groups.length) {
      $("providers").append(
        element(
          "div",
          "添加模型后会按协议、地址和密钥自动建立服务商分组。",
          "empty",
        ),
      );
      return;
    }
    for (const group of groups) {
      const card = element("article", undefined, "model-card");
      card.append(
        element("h3", group.name || group.provider),
        element("p", group.baseUrl, "muted"),
        element(
          "p",
          `${group.modelCount} 个模型 · ${group.enabledCount} 个启用 · ${group.hasKey ? "已保存密钥" : "无需密钥"}`,
          "muted",
        ),
      );
      const names = element(
        "p",
        (group.models || []).map((m) => m.displayName || m.id).join("、"),
        "muted",
      );
      const actions = element("div", undefined, "card-actions");
      actions.append(
        action("编辑整组", () => {
          if (busy) return;
          currentGroup = group;
          const f = $("group-form");
          f.reset();
          f.elements.baseUrl.value = group.baseUrl;
          f.elements.apiKey.disabled = true;
          $("group-error").hidden = true;
          $("group-description").textContent =
            `将更新 ${group.modelCount} 个模型的连接配置。`;
          $("group-editor").showModal();
        }),
        action(group.enabled ? "全部停用" : "全部启用", async () => {
          if (busy) return;
          busy = true;
          try {
            await api(
              `/api/bridge/providers/${encodeURIComponent(group.id)}`,
              "PATCH",
              { enabled: !group.enabled },
            );
            await refresh();
            await loadProviders();
          } catch (e) {
            notice(safeError(e));
          } finally {
            busy = false;
          }
        }),
      );
      card.append(names, actions);
      $("providers").append(card);
    }
  } catch (e) {
    $("providers").replaceChildren(
      element("div", "服务商列表读取失败，请刷新重试。", "empty"),
    );
    notice(safeError(e));
  }
}
$("add-provider").onclick = () => edit();
function closeGroup() {
  if (busy) return;
  $("group-editor").close();
  $("group-form").reset();
  $("group-form").elements.apiKey.value = "";
  currentGroup = null;
}
$("cancel-group").onclick = closeGroup;
$("group-editor").addEventListener("cancel", (e) => {
  e.preventDefault();
  closeGroup();
});
$("group-form").elements.keyMode.onchange = () => {
  const f = $("group-form").elements;
  f.apiKey.value = "";
  f.apiKey.disabled = f.keyMode.value !== "replace";
};
$("group-form").onsubmit = async (e) => {
  e.preventDefault();
  if (busy || !currentGroup) return;
  const f = e.target.elements;
  const body = { baseUrl: f.baseUrl.value.trim().replace(/\/$/, "") };
  try {
    const url = new URL(body.baseUrl);
    if (
      !["http:", "https:"].includes(url.protocol) ||
      url.username ||
      url.password ||
      url.search ||
      url.hash
    )
      throw new Error("接口地址必须是无凭据的 HTTP(S) 地址。");
    if (f.keyMode.value === "replace") {
      if (!f.apiKey.value.trim()) throw new Error("请输入新密钥。");
      body.apiKey = f.apiKey.value.trim();
    } else if (f.keyMode.value === "clear") body.apiKey = "";
    else if (
      currentGroup.hasKey &&
      body.baseUrl !== currentGroup.baseUrl.replace(/\/$/, "")
    )
      throw new Error("地址已改变，请重新输入或明确清空密钥。");
  } catch (err) {
    $("group-error").textContent = err.message;
    $("group-error").hidden = false;
    return;
  }
  busy = true;
  $("save-group").disabled = true;
  try {
    await api(
      `/api/bridge/providers/${encodeURIComponent(currentGroup.id)}`,
      "PATCH",
      body,
    );
    busy = false;
    closeGroup();
    await refresh();
    await loadProviders();
    notice("服务商分组已更新。");
  } catch (err) {
    $("group-error").textContent = safeError(err);
    $("group-error").hidden = false;
  } finally {
    busy = false;
    $("save-group").disabled = false;
  }
};

let accioInfo = null;
$("discover").onclick = async () => {
  const button = $("discover");
  button.disabled = true;
  $("discovered").textContent = "正在探测本机服务…";
  try {
    const result = await api("/api/bridge/discover");
    $("discovered").replaceChildren();
    if (!result.ok) throw new Error("scan failed");
    if (!result.services?.length)
      $("discovered").textContent =
        "未发现服务。请先启动 Ollama、LM Studio 等本机服务。";
    for (const service of result.services || []) {
      const row = element("div", undefined, "panel");
      row.append(
        element("h3", service.name || service.kind),
        element("p", service.baseUrl, "muted"),
      );
      for (const name of service.models || [])
        row.append(
          action(`添加 ${name}`, () => {
            edit();
            const f = $("model-form").elements;
            f.id.value = name;
            f.displayName.value = name;
            f.upstreamModel.value = name;
            f.baseUrl.value = service.baseUrl;
            f.provider.value =
              service.provider === "ollama" ? "ollama" : "openai-compatible";
            f.keyMode.value = "clear";
            f.apiKey.disabled = true;
          }),
        );
      $("discovered").append(row);
    }
  } catch (e) {
    $("discovered").textContent = "扫描失败，请确认网关连接后重试。";
  } finally {
    button.disabled = false;
  }
};
$("inspect-accio").onclick = async () => {
  const button = $("inspect-accio");
  button.disabled = true;
  $("launch-accio").disabled = true;
  $("accio-status").textContent = "正在检查…";
  accioInfo = null;
  try {
    const result = await api("/api/bridge/accio");
    accioInfo = result.accio;
    if (!accioInfo?.installed) {
      $("accio-status").textContent =
        "未检测到 Accio 安装。请安装后重试；自定义安装路径可在网关配置中设置。";
    } else {
      $("accio-status").textContent =
        `已检测安装 · ${accioInfo.editionLabel || accioInfo.edition || "版本未知"} · ${accioInfo.running ? "正在运行" : "未运行"}。账户目录信息不是登录验证。`;
      $("launch-accio").disabled = false;
    }
  } catch (e) {
    $("accio-status").textContent = "检查失败，请确认连接后重试。";
  } finally {
    button.disabled = false;
  }
};
$("launch-accio").onclick = () => {
  if (!accioInfo?.installed || busy) return;
  $("force-launch").checked = false;
  $("launch-warning").textContent = accioInfo.running
    ? "检测到已有 Accio 进程；新进程可能不会接管已有窗口。"
    : accioInfo.edition === "cn"
      ? "已识别为中国版；仍需在目标机器验证接入效果。"
      : "发行版本未知或非中国版，可能不兼容；默认拒绝启动，须显式勾选风险确认。";
  $("launch-dialog").showModal();
};
$("cancel-launch").onclick = () => {
  if (!busy) $("launch-dialog").close();
};
$("launch-dialog").addEventListener("cancel", (e) => {
  if (busy) e.preventDefault();
});
$("confirm-launch").onclick = async () => {
  if (busy) return;
  busy = true;
  $("confirm-launch").disabled = true;
  try {
    const result = await api("/api/bridge/accio/launch", "POST", {
      force: $("force-launch").checked,
    });
    if (!result.ok || !result.launched) throw new Error("launch failed");
    $("launch-dialog").close();
    notice(
      "已发出启动请求。请在 Accio 窗口确认模型可用；尚未执行真实模型测试。",
    );
  } catch (e) {
    $("launch-warning").textContent =
      "启动未完成。请检查安装和发行版本；已有进程或未知版本需要勾选风险确认。";
  } finally {
    busy = false;
    $("confirm-launch").disabled = false;
  }
};
