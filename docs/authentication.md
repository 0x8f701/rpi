# Authentication

`pi` resolves credentials in a strict, documented order and never logs them.
Credentials can be managed interactively with `pi login` / `pi logout` or
configured manually in `auth.json` or `models.json`.

## Quick credential management

```sh
# Interactive login (choose provider and method)
pi login

# Login to a specific provider
pi login anthropic

# Remove stored credentials
pi logout openai
```

`login` and `logout` operate on the agent's `auth.json`. Outside an
interactive terminal they require an explicit provider argument. `auth.json` is
written atomically with `0o600` file permissions and `0o700` directory
permissions on Unix.

## Credential precedence (per provider)

When a request is built, `pi` looks for a usable credential in this order
(source: `crates/pi-cli/src/models_config.rs::resolve_model_request_auth` and
`resolve_model_request_auth_async`):

1. An explicit API key passed at call time, e.g. `--api-key` on the CLI
   (`--api-key` requires `--model` or `--models`).
2. A runtime key set for the provider (the CLI stores `--api-key` as a runtime
   key for the resolved provider for the duration of the run).
3. A stored `auth.json` credential for the provider:
   - `api_key` entries are expanded and used as the key.
   - OAuth entries require async resolution; `pi` refreshes expired OAuth
     tokens automatically before use.
4. The `apiKey` field from `models.json` for that provider.
5. A recognized provider environment variable (see table below).
6. Anthropic-specific bearer handling: when the API-key slot is still empty,
   `ANTHROPIC_AUTH_TOKEN` is sent as `Authorization: Bearer <token>` and is
   never treated as an `x-api-key` value.
7. Provider-specific header-only auth configured in `models.json` or model
   headers, e.g. `authorization`, `x-goog-api-key`, or `cf-aig-authorization`.

If none of the above produce a key or recognized auth header, the request
fails closed with an error naming the provider.

All header-name lookups are case-insensitive. Header and credential values
support `$VAR` / `${VAR}` template expansion (see
[Template expansion](#template-expansion)).

## Supported OAuth providers

`pi login` can store OAuth credentials for:

- `anthropic`
- `openai-codex`
- `google-gemini-cli`
- `xai`
- `openrouter`
- `kimi-coding`

Other providers default to API-key authentication. OAuth tokens are refreshed
automatically when expired (with a 5-minute skew). OAuth credentials may carry
model entitlements (`available_model_ids`); models outside those entitlements
are hidden for that credential.

## Environment variables by provider

| Provider(s) | Variable(s) |
|-------------|-------------|
| `anthropic` | `ANTHROPIC_API_KEY`, `ANTHROPIC_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN` |
| `github-copilot` | `COPILOT_GITHUB_TOKEN` |
| `openai`, `openai-codex` | `OPENAI_API_KEY` |
| `azure-openai-responses` | `AZURE_OPENAI_API_KEY` |
| `google` | `GEMINI_API_KEY` |
| `google-vertex` | `GOOGLE_CLOUD_API_KEY` (or access-token/authorization header) |
| `groq` | `GROQ_API_KEY` |
| `cerebras` | `CEREBRAS_API_KEY` |
| `xai` | `XAI_API_KEY` |
| `deepseek` | `DEEPSEEK_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |
| `nvidia` | `NVIDIA_API_KEY` |
| `mistral` | `MISTRAL_API_KEY` |
| `minimax`, `minimax-cn` | `MINIMAX_API_KEY`, `MINIMAX_CN_API_KEY` |
| `moonshotai`, `moonshotai-cn` | `MOONSHOT_API_KEY` |
| `huggingface` | `HF_TOKEN` |
| `fireworks` | `FIREWORKS_API_KEY` |
| `together` | `TOGETHER_API_KEY` |
| `opencode`, `opencode-go` | `OPENCODE_API_KEY` |
| `kimi-coding` | `KIMI_API_KEY` |
| `cloudflare-workers-ai`, `cloudflare-ai-gateway` | `CLOUDFLARE_API_KEY` |
| `amazon-bedrock` | `AWS_PROFILE`, `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`, `AWS_BEARER_TOKEN_BEDROCK`, `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`, `AWS_CONTAINER_CREDENTIALS_FULL_URI`, `AWS_WEB_IDENTITY_TOKEN_FILE` |
| `ant-ling` | `ANT_LING_API_KEY` |
| `qwen-token-plan`, `qwen-token-plan-cn` | `QWEN_TOKEN_PLAN_API_KEY`, `QWEN_TOKEN_PLAN_CN_API_KEY` |
| `zai`, `zai-coding-cn` | `ZAI_API_KEY`, `ZAI_CODING_CN_API_KEY` |
| `xiaomi`, `xiaomi-token-plan-cn`, `xiaomi-token-plan-ams`, `xiaomi-token-plan-sgp` | `XIAOMI_API_KEY`, `XIAOMI_TOKEN_PLAN_CN_API_KEY`, `XIAOMI_TOKEN_PLAN_AMS_API_KEY`, `XIAOMI_TOKEN_PLAN_SGP_API_KEY` |
| `radius` | `RADIUS_API_KEY` |
| `vercel-ai-gateway` | `AI_GATEWAY_API_KEY` |

Provider-specific notes:

- **Anthropic**: `ANTHROPIC_OAUTH_TOKEN` wins over `ANTHROPIC_API_KEY` when an
  API key is requested. `ANTHROPIC_AUTH_TOKEN` is used as a bearer token in
  the `Authorization` header only when the API-key slot is empty, and is
  never used as an `x-api-key` value.
- **GitHub Copilot**: `COPILOT_GITHUB_TOKEN` is required. The provider adds
  dynamic per-request headers (`X-Initiator`, `Openai-Intent`, and
  `Copilot-Vision-Request` when images are present).
- **Amazon Bedrock**: ambient AWS credentials are intentionally not read.
  When one of the listed AWS credential sources is present, the resolved key
  is the sentinel `<authenticated>` and the AWS SDK signs the request.
- **Google Vertex**: `GOOGLE_CLOUD_API_KEY` supplies an API key, or
  `GOOGLE_CLOUD_ACCESS_TOKEN` / an `authorization` header can supply an access
  token. The provider never reads Application Default Credential files or
  credential helpers. Vertex requests also require `GOOGLE_CLOUD_PROJECT`
  (alias `GCLOUD_PROJECT`) and `GOOGLE_CLOUD_LOCATION`.

## `auth.json`

`auth.json` lives next to `models.json` in the agent directory
(`$PI_CODING_AGENT_DIR`, or the default agent directory under your home).
Example:

```json
{
  "openai": {
    "type": "api_key",
    "key": "$OPENAI_API_KEY",
    "env": {
      "OPENAI_API_KEY": "<your-openai-key>"
    }
  }
}
```

Rules:

- `type` is `"api_key"` for manually edited entries. `pi login` may write
  `"oauth"` entries; those should not be hand-edited.
- `key` is the credential value or a `$VAR` / `${VAR}` template.
- `env` is an optional map of variables available only while resolving this
  credential.
- `$$` becomes a literal `$`; `$!` becomes a literal `!`.
- Command-valued keys (`"key": "!some-command"`) are rejected.

## `models.json` authentication

Custom and overridden providers in `models.json` can carry authentication
independently of `auth.json`:

```json
{
  "providers": {
    "my-proxy": {
      "baseUrl": "https://api.example.com/v1",
      "api": "openai-completions",
      "apiKey": "$MY_PROXY_KEY",
      "authHeader": true,
      "headers": {
        "x-custom": "$CUSTOM_HEADER"
      },
      "models": [
        {
          "id": "my-model",
          "maxTokens": 4096
        }
      ]
    }
  }
}
```

Rules:

- `authHeader: true` sends `Authorization: Bearer <apiKey>`. It requires a
  resolved API key; if none exists the request fails closed.
- Provider-level `headers` apply to every model of that provider; per-model
  `headers` override them. Merging is case-insensitive last-wins.
- `apiKey`, `baseUrl`, and `header` values support `$VAR` / `${VAR}` expansion
  from the process environment. `models.json` does **not** support a per-entry
  `env` map (only `auth.json` credentials do). `$$` is a literal `$` and `$!`
  is a literal `!`. Unset variables produce an error.
- Command-valued `apiKey` or header values (`"!command"`) are rejected.

## Template expansion

Both `auth.json` and `models.json` values use the same expander
(source: `crates/pi-coding/src/auth.rs::expand_credential_value` and
`crates/pi-cli/src/models_config.rs::resolve_config_value_with_fallback`):

| Form | Meaning |
|------|---------|
| `$VAR` | Expand a single variable. |
| `${VAR}` | Explicit boundary. |
| `$$` | Literal `$`. |
| `$!` | Literal `!`. |

For `auth.json`, variables are resolved from the request environment, then the
credential's own `env` map, then the process environment. Empty values are
treated as unset. Invalid braced names such as `${1bad}` are left unchanged.
If a referenced variable is not set, `pi` exits with an error that names the
variable and source file but never exposes the original template text.

## Redaction and sensitive headers

The following headers are treated as sensitive and are marked with
`set_sensitive(true)` so they are hidden from request traces and logs:

- `authorization`
- `x-api-key`
- `cf-aig-authorization`
- `x-goog-api-key`

When an error message is produced by `pi_messages`, `radius`, `mistral`, or
similar providers, the raw API key and any bearer-token values found in the
above headers are stripped from the message before it is surfaced. Error
messages name the missing variable or provider, never the key value.

## Fail-closed behavior

`pi` fails loudly rather than silently when authentication cannot be resolved:

- No API key, no OAuth credential, no recognized auth header, and no
  `authHeader` fallback → request fails with:
  `no API key found for provider ...`.
- `authHeader: true` without a resolved API key → request fails.
- A `$VAR` reference in `auth.json` or `models.json` resolves to an unset
  variable → request fails.
- Azure OpenAI requests with zero or more than one of `authorization` / `api-key`
  → request fails.
- Google Vertex requests with both `authorization` and `x-goog-api-key` in the
  same scope → request fails.
- Amazon Bedrock requests with only one of `AWS_ACCESS_KEY_ID` or
  `AWS_SECRET_ACCESS_KEY` → request fails.

Do not commit `auth.json` or `models.json` containing real keys.
