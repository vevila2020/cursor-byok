# Cursor BYOK

A local model gateway that connects Cursor to your own OpenAI-compatible, Anthropic-compatible, and native Gemini APIs.

**Author: Elias**

[简体中文](./README-CN.md) · [Download](https://github.com/vevila2020/cursor-byok/releases/latest) · [Report an issue](https://github.com/vevila2020/cursor-byok/issues)

## Features

- Connect your own API endpoints, keys, and model IDs.
- Support OpenAI Responses, OpenAI Chat Completions, Anthropic Messages, and Gemini GenerateContent APIs.
- Add, edit, group, reorder, and test model configurations.
- Use Cursor Agent tool calling, Skills, MCP, and multi-turn conversations.
- Inspect request details, token usage, cache hits, and connection performance.
- Run on Windows, macOS, and Linux.

![Model configuration](./images/en-model-1.png)

## Quick start

1. Download the installer for your system from [GitHub Releases](https://github.com/vevila2020/cursor-byok/releases/latest).
2. Launch Cursor BYOK, open **Model Configuration → Cursor**, and initialize the local CA certificate when prompted.
3. Add a model and select the **EliasApi** preset, or enter your provider's Base URL manually.
4. Under **API Key**, click **Get API key** to open [the key management page](https://subapi.elias.kdns.fr/keys), then copy your key into the field.
5. Choose the API protocol and enter a model ID, display name, and remark. You can also use **Fetch models** to load available model IDs.
6. Click **Save and test**. After the test succeeds, enable Cursor integration and keep Cursor BYOK running.
7. Fully quit and restart Cursor, start a new conversation, and select your configured model.

Restart Cursor after its first configuration or an upgrade. Select your model explicitly: **Auto** uses Cursor's official models.

You can also enter another provider's Base URL and API key manually. The **Get API key** button is also available in group settings; changes take effect after saving.

## API configuration

| Model family | Model type | API protocol |
| --- | --- | --- |
| GPT / OpenAI | OpenAI | Responses API |
| Other OpenAI-compatible models | OpenAI | Chat Completions API |
| Claude / Anthropic-compatible models | Anthropic | Messages API |
| Native Gemini models | Gemini | GenerateContent (streaming) |

For a native Gemini channel, enter its server address or its full model list address ending in `/v1beta/models`. Enter the channel API key, then choose **Fetch models** or type the model ID. Use **Save and test** to verify streaming.

- **Base URL:** Use the provider's base address. The application appends the standard path for the selected protocol.
- **Full request URL:** Enable this option only when supplying a complete endpoint URL.
- **API Key:** Enter the key issued by your provider.
- **Model ID:** Use the exact identifier accepted by the provider.
- **Display name and remark:** Set the labels shown in Cursor.

Context limits, output limits, reasoning effort, custom headers, and extra request parameters can be configured for each model.

Keys and settings are stored locally. Model requests are sent to the API service you configure, which determines available models and usage charges.

## How it works

```text
Cursor client
    │ Agent requests and tool results
    ▼
Cursor BYOK local service
    │ OpenAI- / Anthropic-compatible requests
    ▼
Your configured model API
    │ Model responses and tool calls
    └──────────────────────────────► Cursor, through the local service
```

## Project structure

```text
cursor-byok/
├── apps/desktop/          # React interface and Tauri desktop application
│   ├── src/features/     # Model configuration, usage, calls, and settings
│   ├── src/shared/       # Shared controls, API client, and application state
│   ├── src/i18n/         # Localization runtime and translations
│   └── src-tauri/        # Native window, lifecycle, and application updates
├── server/               # Local gateway, protocol adaptation, and persistence
├── crates/semble-core/   # Local code indexing and search
├── protocols/cursor/     # Cursor protocol definitions
├── support/              # Protocol tools and benchmarks
└── .github/workflows/    # Continuous integration and release builds
```

## Local development

Install Node.js 22.12 or later, Rust stable, and the [Tauri system prerequisites](https://v2.tauri.app/start/prerequisites/).

```bash
npm --prefix apps/desktop ci
npm --prefix apps/desktop run tauri:dev
```

Check types and build the frontend:

```bash
npm --prefix apps/desktop run check
```

Build the desktop application without an installer:

```bash
npm --prefix apps/desktop run tauri:build -- --debug --no-bundle
```

Run `make check` for workspace formatting, Clippy, Rust tests, and frontend checks. Installers and signed updater artifacts are built by the GitHub Actions release workflow.

## Independent built-in plugin updates

The desktop app checks for Codex, Grok, and Google plugin updates at startup. Users can install each update from **Plugin Configuration**. Restart the app to activate an installed update. This feature requires a desktop app version that includes plugin updates.

To release one plugin, update `version` in `server/plugins/build-in/<plugin-directory>/plugin.json`, push the commit to GitHub, and create a matching tag on that commit. Use `plugin-<plugin-directory>-v<version>`, for example:

```bash
git tag plugin-codex-auth-v0.1.10
git push origin plugin-codex-auth-v0.1.10
```

GitHub Actions tests and packages that plugin, signs it with the desktop updater signing key, and publishes a GitHub prerelease. Only the repository owner can trigger publication. Plugin prereleases do not replace the latest desktop app release.

## License

[MIT](./LICENSE).
