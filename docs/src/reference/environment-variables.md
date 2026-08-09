# Environment variables

This page lists every environment variable that `rpi` reads from source. Values
are never documented here; set them in your shell or agent configuration. The
lists are grouped by function rather than priority.

## Installer / updater

| Variable | Default | Purpose |
|----------|---------|---------|
| `PI_HOME` | Auto-detected from the running executable layout | Binary install root used by the self-updater |
| `PI_UPDATE_BASE_URL` | `https://api.github.com/repos/0x8f701/rpi/releases` | Release API endpoint for update checks and self-update |
| `GITHUB_TOKEN` | (none) | Authenticate GitHub API calls when the updater hits `api.github.com` |
| `PI_OFFLINE` | (none) | Disables updater networking, llama.cpp router refresh, and other non-essential network calls when set to `1`, `true`, or `yes` |
| `PI_SKIP_VERSION_CHECK` | (none) | Disables only the nonfatal interactive startup version check when set |

## Runtime configuration

| Variable | Default | Purpose |
|----------|---------|---------|
| `PI_CODING_AGENT_DIR` | Default agent directory under the user's home | Agent config directory (`models.json`, `auth.json`, resources, llama cache) |
| `SESSIONS_HOME` | `$HOME` | Relocates the native session subtree to `$SESSIONS_HOME/.pi/agent/sessions` (with the same `~` expansion and absolute-path handling as the catalog) |
| `PI_PACKAGE_DIR` | (none) | Overrides upstream-style package/docs path discovery |
| `PI_SHARE_VIEWER_URL` | (none) | Viewer URL template for shared sessions; `{url}` substitutes the gist URL |

`PI_CODING_AGENT_DIR` is the only way to relocate the agent tree; the current
working directory is never trusted as a fallback. `SESSIONS_HOME` relocates
only the session subtree — agent configuration, skills, and resources remain
under `PI_CODING_AGENT_DIR` (or `$HOME`) when only `SESSIONS_HOME` is set.
Precedence for the session store root is `PI_CODING_AGENT_DIR` >
`$SESSIONS_HOME/.pi/agent` > `$HOME/.pi/agent`.

## Local model configuration

| Variable | Purpose |
|----------|---------|
| `LLAMA_BASE_URL` | llama.cpp router base URL (without `/v1`) |
| `LLAMA_API_KEY` | Optional llama.cpp router bearer token |
| `HF_TOKEN` | Hugging Face token for GGUF search/download |
| `HF_TOKEN_PATH` | Path to a file containing a Hugging Face token |
| `HF_HOME` | Hugging Face cache home; token is read from `token` inside it |
| `HF_ENDPOINT` | Override the Hugging Face base URL |
| `XDG_CACHE_HOME` | Cache home; token is read from `huggingface/token` inside it |
| `HOME` / `USERPROFILE` | User home directory; used for cache and agent directory fallbacks |

## Tool environment

The `bash` tool receives the parent process environment with these additions:

| Variable | Purpose |
|----------|---------|
| `PI_PROVIDER` | Resolved provider id |
| `PI_MODEL` | Resolved model id |
| `PI_REASONING_LEVEL` | Current thinking level name |
| `PI_SESSION_ID` | Current session id |
| `PI_SESSION_FILE` | Path to the current session file |

Parent-process values for these keys are stripped before the additions are
applied, so stale values never leak into a child.

## Provider API keys

These variables are read by `rpi` to authenticate provider requests. Empty values
are treated as unset.

| Variable(s) | Provider |
|-------------|----------|
| `ANTHROPIC_API_KEY`, `ANTHROPIC_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN` | Anthropic |
| `COPILOT_GITHUB_TOKEN` | GitHub Copilot |
| `OPENAI_API_KEY` | OpenAI, OpenAI Codex |
| `AZURE_OPENAI_API_KEY` | Azure OpenAI Responses |
| `GEMINI_API_KEY` | Google Gemini |
| `GOOGLE_CLOUD_API_KEY` | Google Vertex (API-key auth) |
| `GOOGLE_CLOUD_ACCESS_TOKEN` | Google Vertex (access-token auth) |
| `GROQ_API_KEY` | Groq |
| `CEREBRAS_API_KEY` | Cerebras |
| `XAI_API_KEY` | xAI |
| `DEEPSEEK_API_KEY` | DeepSeek |
| `OPENROUTER_API_KEY` | OpenRouter |
| `NVIDIA_API_KEY` | NVIDIA |
| `MISTRAL_API_KEY` | Mistral |
| `MINIMAX_API_KEY`, `MINIMAX_CN_API_KEY` | MiniMax |
| `MOONSHOT_API_KEY` | Moonshot |
| `HF_TOKEN` | Hugging Face |
| `FIREWORKS_API_KEY` | Fireworks |
| `TOGETHER_API_KEY` | Together |
| `OPENCODE_API_KEY` | OpenCode |
| `KIMI_API_KEY` | Kimi coding |
| `CLOUDFLARE_API_KEY` | Cloudflare Workers AI / AI Gateway |
| `AWS_PROFILE` | Amazon Bedrock |
| `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` | Amazon Bedrock (SigV4) |
| `AWS_SESSION_TOKEN` | Amazon Bedrock session token |
| `AWS_BEARER_TOKEN_BEDROCK` | Amazon Bedrock bearer auth |
| `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` | Amazon Bedrock container credentials |
| `AWS_CONTAINER_CREDENTIALS_FULL_URI` | Amazon Bedrock container credentials |
| `AWS_WEB_IDENTITY_TOKEN_FILE` | Amazon Bedrock web-identity credentials |
| `ANT_LING_API_KEY` | Ant Ling |
| `QWEN_TOKEN_PLAN_API_KEY`, `QWEN_TOKEN_PLAN_CN_API_KEY` | Qwen token-plan |
| `ZAI_API_KEY`, `ZAI_CODING_CN_API_KEY` | ZAI |
| `XIAOMI_API_KEY`, `XIAOMI_TOKEN_PLAN_CN_API_KEY`, `XIAOMI_TOKEN_PLAN_AMS_API_KEY`, `XIAOMI_TOKEN_PLAN_SGP_API_KEY` | Xiaomi |
| `RADIUS_API_KEY` | Radius |
| `AI_GATEWAY_API_KEY` | Vercel AI Gateway |

Precedence notes:

- **Anthropic**: `ANTHROPIC_OAUTH_TOKEN` wins over `ANTHROPIC_API_KEY`. `ANTHROPIC_AUTH_TOKEN` is used as a bearer header and is never returned as an API key.
- **Amazon Bedrock**: ambient AWS credentials (shared config files, instance metadata, etc.) are intentionally not read. Only the listed environment variables are considered.
- **Google Vertex**: ADC files and credential helpers are not read. Use `GOOGLE_CLOUD_API_KEY`, `GOOGLE_CLOUD_ACCESS_TOKEN`, or an `authorization` header.

## Azure OpenAI configuration

In addition to `AZURE_OPENAI_API_KEY`:

| Variable | Purpose |
|----------|---------|
| `AZURE_OPENAI_API_VERSION` | API version, default `v1` |
| `AZURE_OPENAI_BASE_URL` | Base URL override |
| `AZURE_OPENAI_RESOURCE_NAME` | Resource name; builds `https://{name}.openai.azure.com/openai/v1` |
| `AZURE_OPENAI_DEPLOYMENT_NAME_MAP` | Comma-separated `modelId=deploymentName` mappings |

## AWS Bedrock configuration

| Variable | Purpose |
|----------|---------|
| `AWS_REGION` | Bedrock region |
| `AWS_DEFAULT_REGION` | Fallback region |
| `AWS_BEDROCK_SKIP_AUTH` | Set to `1` to skip auth (test/local only) |
| `AWS_BEDROCK_FORCE_CACHE` | Set to `1` to force prompt caching |

See the provider API-keys table above for the credential variables.

## Google Vertex configuration

| Variable | Purpose |
|----------|---------|
| `GOOGLE_CLOUD_PROJECT` | Required project id (alias `GCLOUD_PROJECT`) |
| `GOOGLE_CLOUD_LOCATION` | Required location |
| `GOOGLE_CLOUD_ACCESS_TOKEN` | Short-lived access token |
| `GOOGLE_CLOUD_API_KEY` | API key (sent as `x-goog-api-key`) |

## Anthropic cache retention

| Variable | Purpose |
|----------|---------|
| `PI_CACHE_RETENTION` | Set to `long` to request 24h prompt-cache retention for Anthropic and OpenAI Responses models |

## Cloudflare placeholders

For Cloudflare Workers AI and AI Gateway `models.json` `baseUrl` values may
contain:

```text
{CLOUDFLARE_ACCOUNT_ID}  — from env CLOUDFLARE_ACCOUNT_ID
{CLOUDFLARE_GATEWAY_ID}  — from env CLOUDFLARE_GATEWAY_ID
```

They are replaced at request time before the URL is used. If a placeholder is
present and the matching variable is unset, the request fails.

## OAuth tuning

| Variable | Purpose |
|----------|---------|
| `PI_OAUTH_CALLBACK_HOST` | Override the OAuth redirect callback bind address (default `127.0.0.1`) |
| `KIMI_CODE_OAUTH_HOST` | Override the Kimi Code OAuth host |
| `KIMI_OAUTH_HOST` | Fallback override for the Kimi OAuth host |

## Extension host

| Variable | Purpose |
|----------|---------|
| `PI_EXTENSION_ID` | Extension id passed to extensions (internal) |
| `PI_EXTENSION_ENTRY` | Extension entry path (internal) |
| `PI_EXTENSION_PACKAGE_ID` | Package id for the extension (internal) |
| `PI_EXTENSION_CAPABILITIES` | JSON capabilities list (internal) |
| `PI_EXTENSION_UI_CAPABILITIES` | JSON UI capabilities list (internal) |
| `PI_EXTENSION_MAX_FRAME_BYTES` | Max extension IPC frame size (internal) |
| `PI_EXTENSION_PROTOCOL_VERSION` | Extension protocol version (internal) |

## Session import

| Variable | Purpose |
|----------|---------|
| `CODEX_HOME` | Codex directory under the user's home; session import source |
| `CLAUDE_CONFIG_DIR` | Claude config directory under the user's home; project import source |

## Editor / clipboard / display

| Variable | Purpose |
|----------|---------|
| `VISUAL` | Preferred external editor |
| `EDITOR` | Fallback external editor |
| `DISPLAY` | X11 display detection for clipboard |
| `WAYLAND_DISPLAY` | Wayland display detection |
| `XDG_SESSION_TYPE` | Session type detection (e.g. `wayland`) |
| `COLORFGBG` | Terminal background-color hint for theme selection |
| `XDG_CONFIG_HOME` | Used to locate `git/ignore` and other config fallbacks |

## Template expansion

`auth.json` and `models.json` values may use these forms:

| Form | Meaning |
|------|---------|
| `$VAR` | Expand a single variable from the process environment |
| `${VAR}` | Explicit boundary |
| `$$` | Literal `$` |
| `$!` | Literal `!` |

For `auth.json` credentials, variables are also resolved from the optional
per-credential `env` map. `models.json` values use only the process
environment (and, for OAuth-stored credentials, the credential's own `env`).
Unset variables produce an error; there is no default-value syntax.
Command-valued values (`!command`) are rejected for API keys and headers.
