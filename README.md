# Accio Gateway Bridge · Rust 重构版

这是对原 `accio-gateway-bridge` 的独立 Rust 重构：**Rust 网关核心 + 新 Tauri 桌面壳/UI**。原 Node 项目保留为兼容性对照，不覆盖用户现有配置。

**当前验证边界：**已在临时 Linux 环境编译和使用本机模拟上游测试；没有使用真实 Accio 账号、真实模型额度或登录凭据。Windows 安装包/托盘/真实 Accio 联通尚需 Windows 验收。不要把源码构建步骤当成已发布的 Windows 成品。

## 启动网关

安装 Rust 官方工具链后，在本目录执行：

```sh
cargo build --locked --release
```

Windows PowerShell：

```powershell
Copy-Item config.example.json config.json
.\target\release\accio-gateway-bridge-rust.exe --config .\config.json
```

Linux/macOS：

```sh
cp config.example.json config.json
./target/release/accio-gateway-bridge-rust --config config.json
```

默认监听 `127.0.0.1:8080`，不自动调用模型。`--port 0` 自动选择空闲端口，标准输出打印 `{event:"listening",host,port}` JSON。`--help` 查看参数；`ACCIO_BRIDGE_CONFIG` 也可以指定配置路径。配置不存在时使用空模型安全默认值，首次编辑时创建文件，父目录须已存在。

桌面构建和运行见 [`desktop/README.md`](desktop/README.md)（若该文件尚不存在，请参阅桌面模块随附文档）。桌面壳负责自己启动的 Rust 子进程，不再依赖旧 Node daemon。不要把原仓库旧 EXE 当作这个版本的产物。
> **当前在用的桌面入口是 Go 壳，不是本包内的 Tauri 壳。** 本包 `desktop/` 存放的是 Tauri 2 源码，**未启用、从未构建**（官方要求 MSVC 工具链，本机只有 gnu 工具链）；真正在用的是 `target\release\accio-desktop.exe`（Go 壳：7 个视图 + 系统托盘 + 单实例 + `--selftest`），其源码位于原 Node 仓库的 `desktop/` 目录（与本包分开维护）。两种壳都只是外壳，引擎始终是本包的 `accio-gateway-bridge-rust.exe`。

## 原配置迁移

1. **先备份**原 `config.json`，不要覆盖唯一副本。
2. 将副本放入新版本的独立数据目录，通过 `--config` 指向它。
3. 原 `server` / `upstream` / `catalog` / `models` 字段可读取；不认识的字段保存时保留，但不代表其旧功能仍在运行。
4. 确认模型、提供商、默认模型和端口后，用模拟或自有测试模型检查。真实请求可能产生费用，由你决定是否发送。
5. 若回退，关闭新网关，启动原 Node 版本并使用原配置备份。

桌面生成的 `ACCIO_BRIDGE_TOKEN` 是**仅管理接口和健康检查使用的临时令牌**，不会写入配置。它不阻断 Accio 默认数据请求。保存的 `server.token` / `server.enforceToken` 则用于数据接口认证，两者不是同一凭据。

## 功能矩阵

| 功能 | 当前实现与限制 |
|---|---|
| Accio 模型目录 | 自定义模型 + 官方目录合并；有界、按上游/认证/请求体区分的单槽缓存及并发合并 |
| Accio 生成接口 | 三个旧路由别名；模型 ID/别名不区分大小写；OpenAI Chat Completions、Ollama |
| Azure | OpenAI 兼容形状 + `api-key` 请求头；不是完整 Azure deployment/API-version 适配器 |
| 协议转换 | 文本、系统提示、图片输入、工具声明/调用/结果；工具 ID 续轮匹配；非图片附件转为明确的元数据说明，不解码或抓取内容 |
| 流式输出 | 真实分块转发；SSE 多行/CRLF/CR/UTF-8、Ollama NDJSON 尾帧；客户端断开取消上游 |
| 错误处理 | 异常/截断流不伪装成功；模型请求不自动重试或转发另一模型，避免重复计费 |
| 模型/提供商管理 | 增删改（`?id=`、路径式 `DELETE /api/bridge/models/<id>`、`POST /api/bridge/models/delete` 支持 body/query）、分组、默认模型、开关；密钥不回显，省略保留、空串清除 |
| 管理面板 | 网关自身在 `/`、`/index.html`、`/dashboard`、`/dashboard/` 返回内嵌的管理面板（复用旧版面板，`no-store`）；面板对已存密钥只显示 `hasKey`，编辑时**留空即保留**、输入新值即替换、显式清空才删除 |
| 配置管理 | 白名单字段编辑；验证私有副本后原子落盘；运行时管理令牌不持久化 |
| 日志/用量 | 与 Node 版同形的有界日志（每行 `ts`/`level`/`msg`，另带 `model`；容量 1000 行）、等级/条数过滤；**记录等级可选**（`debug`/`info`/`warn`/`error`，默认 `info`）：启动时用 `ACCIO_BRIDGE_LOG_LEVEL` 或配置 `logging.level`（兼容旧的 `server.logLevel`），运行时用 `GET`/`POST /api/bridge/logging` 切换；低于阈值的行不入环，`debug` 行记录解析后的上游地址、上游状态、工具数、**客户端回填的工具结果名**与透传去向；请求数、错误数、耗时；每套不同的转发工具集记一条诊断日志；`ACCIO_BRIDGE_CAPTURE_DIR` 可选落盘生成请求；不是费用账单/Token 计费系统 |
| 本地发现 | 显式触发才探测；有界并发/超时/响应体；支持指定提供商查询模型 |
| Accio 检测/启动 | 读取有限安装元数据与版本证据；启动必须显式调用；未知/非中国版要求确认；测试只使用 fixtures，真实启动未验收 |
| 官方透传 | 保留方法/状态/原始请求体及查询字节，去除逐跳头；入站 `cookie`/`authorization`/`x-accio-token` 随请求转发（对齐 Node 版），但绝不转发本网关自己的 `x-gateway-token` |
| Responses / Anthropic 转换 | **未实现**，不会因为参考仓库支持就宣称支持 |
| MCP / DALL-E 图片生成 | **未实现专用转换**；未知路由仅受限官方透传，不等于 MCP/生图功能已验证 |
| 旧 Node CLI/Go WebView2 生命周期 | 不保留完整旧命令集合；由新二进制参数/Tauri 子进程管理替代 |

图片支持是**输入图片协议转换**，模型能否识图由上游决定，不是图片生成接口。

## 安全边界

- 默认仅回环地址；非回环绑定必须配置令牌，所有请求强制认证。外部浏览器 Origin 和不受信 Host 被拒绝。
- 自定义模型只接收它自身保存的 API key，不接收官方 Cookie、官方 Authorization 或网关令牌；禁用上游重定向。
- 官方入站标头按 Node 版语义转发：除逐跳头、重新计算的 `content-length` 与本网关管理令牌 `x-gateway-token` 外全部透传，包含 `cookie` / `authorization` / `x-accio-token`——Accio 的登录态正是这样在 `POST /api/mcp/ali1688/sgcookie` 这类路径上重放的；等于保存的 `server.token` 或运行时令牌的 Bearer 会被过滤，不会当作凭据发给官方上游。
- 查询参数 `token` / `sg_k` 不透传；响应 Cookie 不透传；重定向不跟随。真实可用的认证方式仍以 Accio 自身登录态为准。
- 修改隐藏密钥的目的地址时必须重新输入或明确清除密钥，避免把旧密钥自动发送给新地址。
- 请求、解压后体积、单帧、累计工具参数、JSON 响应均设上限。取消和错误计入诊断。
- 配置仍含**明文 API key**；Unix 新文件使用 0600 权限，不等于加密钥匙串。请勿把真实配置放进 Git、压缩包或日志。

## 验证

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --locked
node tests/parity.cjs
```

Node 只用于兼容性测试脚本，**Rust 网关运行不依赖 Node**。单测包含从已测试 Node 基线生成的 24 组协议黄金样例，以及本机模拟 HTTP/流式/重定向边界；没有真实模型请求。最终测试数量以随附验证报告为准。

## 为什么参考 AI-Helper，而不是照搬

参考了 `TF49/AI-Helper` 的 Accio 子模块设计（MIT），不是整个 BobAPI/账户管理系统。代码审查发现其部分路径先缓冲整个响应再处理、工具 ID 不一致等问题，本版没有采用这些行为。此前无明确根 LICENSE 的 auth 参考仓库只用于设计分析，没有复制其实现。

Rust 是维护与部署方案选择，**不能据此断言比原 Node 更快、更稳定或功能更多**。当前没有性能基准，也没有真实 Windows/账号验收。详细源码对比以随附参考审查报告为准。

## Accio Work 0.37.2 协议增量

与上游 f39fef4 的逐项对照、安全差异和当前验证见 `docs/adk-0372-comparison.md`。

## 管理面板

打开 `http://127.0.0.1:8080/dashboard`（或 `/`）即用，无需另装桌面程序。面板与桌面壳共用同一套管理接口：模型增删改、默认模型与开关、设置、日志/用量、飞轮流式测试、Accio 客户端识别与一键接管。

面板的密钥字段按网关的脱敏接口改写：字段留空表示保留已存密钥，输入新值表示替换，显式清空并保存才是删除——避免"编辑一个字段就把密钥清掉"。

页面本体内嵌在二进制里，源码见 `src/dashboard.html`。
