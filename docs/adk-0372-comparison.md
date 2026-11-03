# Accio Work 0.37.2：f39fef4 逐项对照

核对时间：2026-11-03。成功 fetch 后，上游 `origin/main` 为 `f39fef4e7458416dfc0cd9cfedf15d180791ed06`；对比基线 `f0e7c5598483c319212890332e07714a2dfb7ee0`。来源：https://github.com/bigGGBondsmall/accio-gateway-bridge/commit/f39fef4e7458416dfc0cd9cfedf15d180791ed06 。上游差异涉及 README、converter、proxy、server、mock-openai、integration tests 六个文件。本包是本地 Rust 对应更新，未提交或推送。

此前 `changes-2026-11-03.md` 中“没有新提交”的结论仅对应第一次检查；随后上游出现 f39fef4，本文件取代该旧结论。该文件保留历史接口别名修复说明。

## 功能逐项对照

| 上游改动 | Rust 状态 / 实现 |
|---|---|
| ADK `function_call` / `function_response`、`args_json` / `response_json` | 原已有部分支持；补齐 `call_id` / `tool_call_id` / `function_name`，保留显式 ID 与 FIFO 回填，不引入上游 ID 碰撞回退 |
| 工具结果 `{result/content,is_error}` 包装解包 | 新增。仅对明确的包装键集合解包，含其它业务字段的对象完整保留 |
| 工具 schema 各命名 / JSON 字符串 | 覆盖 parameters、parameters_json、parametersJson、parametersJSON、parametersJsonSchema、parameters_json_schema、parametersSchema、input_schema、inputSchema、input_json、args |
| 工具嵌套声明 | 支持 functionDeclarations/function_declarations、functions、declarations、tools 和 OpenAI function；递归深度限 32 |
| 同名 placeholder 后出现完整 schema | 升级 placeholder，保留已存在的真实 schema；无 schema 使用 additionalProperties=true |
| 历史 thought 内容 | 过滤 `thought:true`，删除只有思考文本或空内容的普通轮次，避免回放为回复 |
| 流式/非流式 reasoning | 独立 `thought:true` part；兼容 reasoning_content/reasoning 及 Ollama thinking |
| includeThoughts / thinkingBudget | false / 0 关闭展示；支持 camel/snake，根字段优先于 generationConfig，然后兼容嵌套 thinkingConfig |
| reasoningEffort | 根字段、properties、generationConfig 的 camel/snake 别名转 reasoning_effort |
| responseFormat / toolChoice / topK | json_object/text 转 response_format.type；toolChoice/topK 转 tool_choice/top_k。tool_choice 仅在有工具时发送。json_schema 字符串缺少完整 schema，明确返回错误，不发送无效上游请求；完整结构化 schema 模式尚未实现。其它未知字符串忽略 |
| 重复 tool index | 流式 index 对应不同 ID 时创建新调用，保留参数片段和此前调用 |
| 畸形工具参数 | 出站 args={}，argsJson 保留原始错误文本，供 Accio 标记解析错误；入站历史无效 JSON 不再污染上游 arguments |
| 图片 | 保留 inline data URL、HTTP(S) 图片 URL，Ollama inline images；不自动抓取 Ollama 远程图片 |
| 非图片附件 | 转为明确的 metadata 文本，标记内容未解码/未抓取；不会假装已读取 PDF 等附件 |
| stream=true 返回 JSON completion | 新增有界 JSON fallback，合法 completion 转 Accio frame；无 message/空 completion 为错误 |
| 自定义模型返回 HTML | 新增错误转换，摘要去除脚本/样式并限长；不会当作正常模型回复 |
| 官方 generation 返回 200 HTML | 仅 generation 路由启用 HTML guard，返回 SSE error；普通 passthrough 保持状态/原始 body |
| gzip / Brotli / deflate 错误页 | 有界解压，含 raw deflate fallback，防解压体积膨胀 |
| 空流、截断流、上游错误 | 不伪造成功 STOP；DONE-only 空流明确 error。保留既有严格结束与错误统计 |
| 请求 capture 目录 | **已移植**：`ACCIO_BRIDGE_CAPTURE_DIR` 只在显式设置时落盘生成请求；未设置时零行为变化。写入失败只记一条 warn，不影响该轮请求 |
| 工具名 / 缺省 schema 的日志诊断 | **已移植**：每套不同的转发工具集记一条日志（数量/名称），并分别标注被丢弃的声明形状、被迫使用宽松 schema 的工具、以及由占位声明升级的工具 |
| 路径式模型删除 | **新增**：`DELETE /api/bridge/models/<id>`（含 `%2F` 编码段、大小写不敏感、未命中 404）。旧 Node 版支持而本包此前返回 501 |
| 管理面板 | **新增**：网关自身在 `/`、`/index.html`、`/dashboard`、`/dashboard/` 返回内嵌的管理面板（复用旧版面板页面，`no-store`），因此不装桌面壳也有可视化入口。面板针对脱敏接口改写密钥字段：留空=保留已存密钥、输入新值=替换、显式清空=删除；旧面板在脱敏接口下会把留空当成空串而清掉密钥，本版已修 |

## 保留的既有增强与安全差异

- 认证、Host/Origin 检查、凭据转发边界、禁用重定向、请求/帧/JSON/解压大小限制保持。
- 保留原子配置保存、隐藏密钥、provider destination/key 保护与 runtime management token 分离。
- 保留稳定工具调用 ID、显式匹配优先、同名 FIFO 匹配与预留 ID 避碰撞；新 Node 上游没有这些完整保护，不能直接替换。
- Ollama 保留独立 images/tool_name 转换，不照搬上游丢失这些字段的实现。
- 现有 HTTP(S) image fileData 仍按图片发送，与新 Node 全部 fileData 转文字不同。
- 请求超时/错误不自动重试或改投其它模型，避免重复计费或重复工具执行。
- earlier compatibility fixes：`/__bridge/accio`、`/__bridge/accio/launch` 与 POST models/delete 别名仍在。

## 验证与边界

最终日志：`validation/f39fef4-final-checks.log`。统一运行入口 `sh scripts/check.sh` 包含格式、单元测试、clippy、核心构建、旧黑盒和新增流传输黑盒；另运行 UI 的 npm test 和 npm run check。

新增协议单测覆盖 schema 各别名/嵌套/升级、snake_case ID 与结果解包、历史思考过滤、思考控制优先级、生成选项、畸形工具参数、非流 reasoning、无效 JSON completion 和附件 metadata。新增传输黑盒覆盖思考开关、重复 index、空流、JSON fallback、自定义/官方 gzip HTML、普通代理透明性。旧 golden 只更新与新版 Node 一致的 top_k 和 permissive schema 预期，保留 FIFO/Ollama 等已有增强。

测试仅使用合成 loopback 上游；没有真实 Accio 账号、真实模型或计费请求。未重跑 Windows / Tauri 原生桌面构建，无 Windows 安装包/原生 GUI 验收。本包不包含 target、node_modules、实际配置、任何旧二进制或 sidecar。示例配置密钥为空；测试内 key/token 为合成数据。

### 最终回归结果

- Rust 单元测试：44/44（新增路径式删除、工具报告诊断、面板托管与面板路径归一化四项）。
- 旧核心黑盒：22/22（新增路径式删除、capture + 工具集日志、面板托管与面板密钥保留四项）。
- 新传输黑盒：通过，含 stream true/false 的空 JSON、thought-only JSON 关闭思考不返回 STOP、嵌套 thinkingConfig 开关。
- 交叉校验：本包二进制另跑对比分支的 28 项黑盒，28/28 通过（其中路径式删除、capture/工具日志为该分支独有用例）。
- 面板实机核对：在真实浏览器打开 `/dashboard` 渲染完整，读到的状态为端口 8080、上游 phoenix-gw.alibaba.com、自定义模型 2/4、Accio 中国版与运行中 PID；模型编辑弹窗对已存密钥显示"已保存密钥（留空 = 保留…）"且不回填明文。
- UI 测试：16/16。
- fmt、clippy（warnings denied）、核心 build、UI 格式检查全部通过。
- 审计修复：SSE 解码器按增量记录帧大小，空 `data:` 行也计入限额，防止无限空行累积和重复计数带来的平方级开销；checked_add 防止计数溢出。
