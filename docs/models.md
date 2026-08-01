# Models and custom providers

## Built-in catalog

`pi` ships with an embedded model catalog in
[`crates/pi-ai/src/models_catalog.json`](crates/pi-ai/src/models_catalog.json).
The catalog is loaded once on first use and restored for built-in provider ids
when a custom `models.json` is reloaded.

List available models with:

```sh
pi models
pi models claude
```

The filter is case-sensitive and matches provider name or model id. The list
also reflects model entitlements from the resolved credential (e.g., GitHub
Copilot OAuth model lists).

## Supported API identifiers

A model's `api` field selects the wire protocol. The identifiers registered by
the built-in provider set are (source:
`crates/pi-ai/src/types.rs` / `crates/pi-ai/src/providers/mod.rs`):

| Identifier | Protocol |
|------------|----------|
| `openai-completions` | OpenAI chat completions (`/chat/completions`) |
| `openai-responses` | OpenAI Responses API (`/responses`) |
| `openai-codex-responses` | OpenAI Codex Responses API |
| `azure-openai-responses` | Azure OpenAI Responses API |
| `anthropic-messages` | Anthropic Messages API |
| `bedrock-converse-stream` | Amazon Bedrock Converse streaming API |
| `google-generative-ai` | Google Gemini API |
| `google-vertex` | Google Vertex AI API |
| `mistral-conversations` | Mistral conversations API |
| `pi-messages` | Dynamic Radius provider catalog |
| `faux` | Deterministic test/example provider |

## Model spec resolution

When you write `pi -m <spec>` or use `/model` in the REPL, `pi` resolves it
with the following algorithm (source: `crates/pi-coding/src/resolve.rs`):

1. If the text before the first `/` matches a known provider id (exactly,
   case-insensitively), the rest is treated as the model id within that
   provider: `anthropic/claude-sonnet-4-5`.
2. Otherwise the whole string is matched as a bare model id across all
   providers. This allows OpenRouter-style ids that contain their own slashes,
   e.g. `openrouter/ai21/jamba-large-1.7`.
3. Case-insensitive substring matching on model `id` and `name`. Aliases (ids
   ending in `-latest` or without a `-YYYYMMDD` date suffix) are preferred over
   dated versions; otherwise the latest dated version by descending id is used.
4. If the provider is known but the id is not, `pi` synthesizes a custom-id
   model by cloning that provider's default template (`defaultModelPerProvider`),
   or the provider's first model if no default is defined. A warning is emitted.

A thinking-level suffix `:off|minimal|low|medium|high|xhigh` is parsed off the
end of the spec. On a custom-id fallback the suffix is stripped from the id
first. The CLI `--thinking` flag accepts `off|minimal|low|medium|high|xhigh|max`
(`max` is an alias for `xhigh`). If the resolved model does not have
`reasoning: true`, only `off` is available; a non-off suffix on a custom
fallback enables `reasoning: true` for that request.

When no spec is given, the default is `anthropic/claude-sonnet-4-5`.

Examples:

```sh
pi -m anthropic/claude-sonnet-4-5
pi -m claude-sonnet-4-5:high
pi -m openrouter/ai21/jamba-large-1.7
pi -m my-provider/my-custom-model:low
pi -m llama.cpp/llama-3.1-8b
```

## Custom providers via `models.json`

Place `models.json` in the agent directory (`$PI_CODING_AGENT_DIR`, or the
default agent directory under your home). If no explicit agent directory or
home directory is available, custom configuration is disabled and the
current working directory is never used as a fallback.

### OpenAI-compatible proxy

```json
{
  "providers": {
    "cliproxy": {
      "baseUrl": "https://api.example.com/v1",
      "api": "openai-completions",
      "models": [
        { "id": "gpt-5.5", "maxTokens": 8192 }
      ]
    }
  }
}
```

### Cloudflare AI Gateway

```json
{
  "providers": {
    "cf-gateway": {
      "baseUrl": "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai/",
      "api": "openai-completions",
      "apiKey": "$CLOUDFLARE_API_KEY",
      "models": [
        { "id": "@cf/moonshotai/kimi-k2.6" }
      ]
    }
  }
}
```

The placeholders `{CLOUDFLARE_ACCOUNT_ID}` and `{CLOUDFLARE_GATEWAY_ID}` are
substituted from the environment at request time. See
[`environment-variables.md`](environment-variables.md).

### Anthropic with a bearer token

```json
{
  "providers": {
    "anthropic": {
      "apiKey": "$ANTHROPIC_AUTH_TOKEN",
      "authHeader": true,
      "models": []
    }
  }
}
```

A provider-level override with an empty `models` array keeps the built-in
Anthropic models unchanged but applies the provider-level `apiKey`,
`authHeader`, `baseUrl`, `headers`, or `compat`.

### Override a built-in model

```json
{
  "providers": {
    "openai": {
      "models": [
        { "id": "gpt-5.5", "maxTokens": 16384 }
      ]
    }
  }
}
```

The per-model entry is merged with the built-in entry; `maxTokens` and other
fields override defaults.

## `models.json` schema

Top level:

```json
{
  "providers": {
    "<provider-id>": {
      "name": "Display name",
      "baseUrl": "https://...",
      "api": "openai-completions",
      "apiKey": "$VAR",
      "authHeader": false,
      "headers": { "x-foo": "$FOO" },
      "compat": { "supportsStrictMode": true },
      "models": [
        {
          "id": "model-id",
          "name": "Display name",
          "api": "openai-completions",
          "baseUrl": "...",
          "reasoning": true,
          "thinkingLevelMap": { "off": null, "high": "high" },
          "input": ["text", "image"],
          "cost": { "input": 3.0, "output": 15.0, "cacheRead": 1.5, "cacheWrite": 0.5 },
          "contextWindow": 200000,
          "maxTokens": 8192,
          "headers": { "x-model": "value" },
          "compat": { "supportsReasoningEffort": false }
        }
      ]
    }
  }
}
```

Rules:

- A provider must define at least one of `baseUrl`, `headers`, `compat`,
  `apiKey`, `authHeader`, or `models`.
- `api` and `baseUrl` can be set at provider level and overridden per model.
  Custom models require an `api` and a `baseUrl` somewhere in the fallback
  chain; otherwise loading fails closed.
- Model-level fields override provider-level fields.
- `reasoning: true` enables the `off|minimal|low|medium|high|xhigh` level ladder;
  without it only `off` is available.
- `thinkingLevelMap` can disable a level (`{ "xhigh": null }`) or map a level
  name to a provider-specific value.
- `compat` is merged. Nested `openRouterRouting`, `vercelGatewayRouting`, and
  `chatTemplateKwargs` objects are merged deeply; all other compat keys are
  replaced.
- For custom model entries, defaults are: `input` `["text"]`,
  `contextWindow` `128000`, `maxTokens` `16384`, empty `cost`, no `compat`.

## The `llama.cpp` provider

When a llama.cpp router is configured (`pi llama configure URL` or
`LLAMA_BASE_URL`), live router models are merged into the catalog under the
provider id `llama.cpp`. Select them with:

```sh
pi -m llama.cpp/llama-3.1-8b
```

At startup `pi` refreshes the live router catalog unless `PI_OFFLINE=1|true|yes`
is set. If the router is unavailable, `pi` falls back to the cached catalog
stored in the agent directory. Router configuration and the cache are written
atomically; the settings file must not be readable by group or other users on
Unix. See [`local-llama.md`](local-llama.md) for router setup and GGUF
downloads.

## GitHub Copilot

`github-copilot` is treated specially:

- It uses `COPILOT_GITHUB_TOKEN` for bearer authentication.
- Before each request it adds dynamic headers based on the context:
  - `X-Initiator: user` when the last message is from the user, otherwise
    `agent`.
  - `Openai-Intent: conversation-edits`.
  - `Copilot-Vision-Request: true` when the context contains image content.
- Models are filtered by the entitlements returned for the resolved credential.

## Capability flags in `compat`

Common flags used by the built-in providers:

| Flag | Effect |
|------|--------|
| `supportsStrictMode` | Enable JSON-schema strict tool sampling |
| `supportsOpenAIGrammarTools` | Enable grammar-style constrained sampling |
| `supportsReasoningEffort` | Map reasoning level to `reasoning_effort` |
| `supportsEagerToolInputStreaming` | Anthropic eager tool-input streaming |
| `supportsLongCacheRetention` | 24h prompt cache retention |
| `supportsCacheControlOnTools` | Cache control on Anthropic tools |
| `supportsDeveloperRole` | OpenAI Responses developer role |
| `supportsStore` | Allow storing OpenAI Responses sessions |
| `allowEmptySignature` | Allow empty thinking signatures |
| `forceAdaptiveThinking` | Force adaptive thinking on Anthropic |
| `maxTokensField` | Rename the `max_tokens` request field |

The full set of flags and defaults is defined in the provider source files
(`crates/pi-ai/src/providers/*.rs`).
