# Cursor BYOK

运行在本机的 Cursor 模型网关，让 Cursor 使用你配置的 OpenAI / Anthropic 兼容 API。

**作者：Elias**

[English](./README.md) · [下载最新版本](https://github.com/vevila2020/cursor-byok/releases/latest) · [反馈问题](https://github.com/vevila2020/cursor-byok/issues)

## 主要功能

- 自定义 API 地址、密钥和模型 ID。
- 支持 OpenAI Responses、OpenAI Chat Completions 和 Anthropic Messages 协议。
- 添加、编辑、分组、排序模型，并测试连接。
- 使用 Cursor Agent 的工具调用、Skills、MCP 和多轮对话。
- 查看请求详情、Token 用量、缓存命中和连接性能。
- 支持 Windows、macOS 和 Linux。

![模型配置](./images/en-model-1.png)

## 快速开始

1. 从 [GitHub Releases](https://github.com/vevila2020/cursor-byok/releases/latest) 下载适合你操作系统的安装包。
2. 启动 Cursor BYOK，打开 **模型配置 → Cursor**，按提示初始化本地 CA 证书。
3. 添加模型，在 **服务器地址（Base URL）** 下点击 **一键写入 Elias 站点**，自动填入 `https://subapi.elias.kdns.fr`。
4. 点击 **获取密钥**，打开[密钥管理页面](https://subapi.elias.kdns.fr/keys)，将获得的密钥填入 **API Key**。
5. 选择请求协议，填写模型名称、显示名称和备注；也可以点击 **获取模型** 读取可用模型列表。
6. 点击 **保存并测试**。测试通过后开启 **接管 Cursor**，并保持 Cursor BYOK 运行。
7. 完全退出并重新启动 Cursor，新建对话，手动选择配置好的模型。

首次配置或升级 Cursor 后需要重启 Cursor。**Auto** 使用 Cursor 官方模型，使用自己的 API 时请明确选择已配置的模型。

也可以手动填写其他服务商的 Base URL 和 API Key。分组设置同样提供站点写入与获取密钥按钮，保存后应用到组内模型。

## 模型配置

| 模型系列 | 模型类型 | 请求协议 |
| --- | --- | --- |
| GPT / OpenAI | OpenAI | Responses API |
| 其他 OpenAI 兼容模型 | OpenAI | Chat Completions API |
| Claude / Anthropic 兼容模型 | Anthropic | Messages API |

- **服务器地址（Base URL）**：填写服务商基础地址，应用会根据所选协议追加标准接口路径。
- **使用完整请求地址**：仅在填写完整接口 URL 时开启。点击 **一键写入 Elias 站点** 会自动关闭此选项。
- **API Key**：填写对应服务商签发的密钥。
- **模型名称**：填写服务商接受的准确模型 ID。
- **显示名称和备注**：设置 Cursor 模型列表中的名称与说明。

每个模型还可以单独设置上下文窗口、最大输出、推理强度、自定义 Headers 和额外请求参数。

密钥和配置保存在本机，模型请求会发送到你配置的 API 服务；可用模型与用量费用由对应服务商决定。

## 工作方式

```text
Cursor 客户端
    │ Agent 请求与工具执行结果
    ▼
Cursor BYOK 本地服务
    │ OpenAI / Anthropic 兼容请求
    ▼
你配置的模型 API
    │ 模型响应与工具调用
    └──────────────────────────► 经本地服务返回 Cursor
```

## 项目结构

```text
cursor-byok/
├── apps/desktop/          # React 界面与 Tauri 桌面应用
│   ├── src/features/     # 模型配置、用量、调用记录与设置
│   ├── src/shared/       # 通用控件、API 客户端与应用状态
│   ├── src/i18n/         # 多语言运行时与翻译
│   └── src-tauri/        # 原生窗口、生命周期与应用更新
├── server/               # 本地网关、协议适配与持久化
├── crates/semble-core/   # 本地代码索引与搜索
├── protocols/cursor/     # Cursor 协议定义
├── support/              # 协议工具与基准测试
└── .github/workflows/    # 持续集成与发版构建
```

## 本地开发

安装 Node.js 22.12 或更新版本、Rust stable，以及 [Tauri 系统构建依赖](https://v2.tauri.app/start/prerequisites/)。

```bash
npm --prefix apps/desktop ci
npm --prefix apps/desktop run tauri:dev
```

检查类型并构建前端：

```bash
npm --prefix apps/desktop run check
```

构建不含安装包的桌面应用：

```bash
npm --prefix apps/desktop run tauri:build -- --debug --no-bundle
```

运行 `make check` 可执行工作区格式检查、Clippy、Rust 测试和前端检查。安装包和签名更新文件由 GitHub Actions 发版工作流生成。

## 许可证

采用 [MIT License](./LICENSE)。
