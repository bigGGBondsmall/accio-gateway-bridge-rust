# Accio Bridge 桌面端（Tauri 2）
> **状态：未启用（保留备选）。** 本包实际使用的桌面入口是 Go 壳 `target\release\accio-desktop.exe`（源码在 Node 仓库 `desktop/`）。本目录的 Tauri 壳需要 MSVC/VS Build Tools：它曾在一台 Linux 云工作区完成原生 `cargo build`（见同目录 `VALIDATION.md`，含当时的 sidecar 哈希），但**从未在 Windows 上构建或启动过**，也没有 Windows GUI 验收记录。本机（只有 gnu 工具链）无法构建它。除非明确要迁移到 Tauri，否则不要把这里当作当前入口。

这是原生 Tauri 窗口 + **随应用打包的 Rust 网关子进程**，不是把网页另起一个名字。启动后随机分配回环端口，生成仅本进程有效的管理令牌，由 Rust 代理允许列表里的管理请求。令牌不传入 JavaScript、不写入配置；关闭应用会终止其网关。API Key 在配置文件中保存，**尚未接入操作系统安全存储**。

## Windows 构建

需要官方 Rust stable（MSVC 工具链）、Visual Studio C++ Build Tools、WebView2、Node.js，以及 Tauri 2 CLI。不要在本地运行来源不明的安装脚本。

```powershell
# 从 rust-port 根目录，在安装好构建前置条件后执行
node desktop/prepare-sidecar.mjs
cd desktop
cargo tauri build
```

Tauri CLI 可通过官方 `cargo install tauri-cli --version '^2' --locked` 安装。打包目标默认 NSIS；输出在 `desktop/target/release/bundle/nsis/`。开发运行同样先准备 sidecar，然后在 desktop 目录执行 `cargo tauri dev`。跨平台需要匹配的系统依赖及 target，不能把 Linux 二进制当作 Windows EXE。

## 界面

模型添加、编辑、启停、删除、设为默认、搜索；服务商分组编辑与整组启停；设置真实连接 `/api/bridge/config`；诊断只请求本地健康与管理端点，不向模型发消息。模型是独立的服务商配置单元，并非一个全局上游；启用的模型在 Accio 目录里供选择。本机服务发现、Accio 安装检查和带二次确认的启动入口只在用户点击后执行；启动使用 GATEWAY_BASE_URL，不修改 Accio 安装。未提供登录、真实付费测试或 MCP 托管。

浏览器开发预览可用静态 HTTP 服务打开 `ui/`，然后输入本机网关端口、当前会话令牌。令牌只驻留内存，刷新/断开即清除。桌面打包 CSP 禁止浏览器直接联网，只允许 Rust IPC。

**验证边界：** WebView2 / Windows 安装包须在 Windows CI 或目标机器实测；源码、DOM 测试或 Linux cargo check 不能代替该验证。任何自动复制 Accio 令牌的功能均未实现。
