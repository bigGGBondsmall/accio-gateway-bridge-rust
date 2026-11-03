export function buildModel(form, original = null) {
  const value = Object.fromEntries(new FormData(form));
  const model = {
    id: value.id.trim(),
    displayName: value.displayName.trim(),
    provider: value.provider,
    baseUrl: value.baseUrl.trim().replace(/\/$/, ""),
    upstreamModel: value.upstreamModel.trim(),
    aliases: value.aliases
      .split(",")
      .map((v) => v.trim())
      .filter(Boolean),
    enabled: form.elements.enabled.checked,
    stream: form.elements.stream.checked,
  };
  const url = new URL(model.baseUrl);
  if (
    !["http:", "https:"].includes(url.protocol) ||
    url.username ||
    url.password ||
    url.search ||
    url.hash
  )
    throw new Error("接口地址必须为 HTTP(S)，不能含用户名、密码、参数或片段。");
  if (!model.id || !model.upstreamModel)
    throw new Error("请填写模型 ID 和上游模型名称。");
  const changed =
    original &&
    (original.baseUrl.replace(/\/$/, "") !== model.baseUrl ||
      original.provider !== model.provider);
  if (value.keyMode === "replace") {
    if (!(value.apiKey ?? "").trim())
      throw new Error("请输入新 API Key，或选择明确清空。");
    model.apiKey = (value.apiKey ?? "").trim();
  } else if (value.keyMode === "clear") model.apiKey = "";
  else if (changed && original.hasKey)
    throw new Error(
      "接口地址或协议已更改：请重新输入密钥，或明确清空，避免密钥发给错误服务。",
    );
  else if (!original) model.apiKey = "";
  return model;
}
export function safeError(error) {
  // Never render upstream errors: they may contain provider credentials or request payloads.
  return error instanceof Error && error.name === "AbortError"
    ? "请求已取消。"
    : "操作未完成。请检查网关连接、输入格式和权限后重试。";
}
