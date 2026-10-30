# Models and custom providers

Select a model with `rpi --model PROVIDER/MODEL` or a bare model identifier. The resolver also accepts a thinking-level suffix on a model specification. Prefer a fully qualified provider/model pair when names overlap.

## Discover and select

`rpi models [FILTER]` lists available models grouped by provider. The optional substring matches the provider or model identifier case-sensitively. Results are sorted; no matches produce no model rows. `--list-models [SEARCH]` is also available on the run command.

Availability is filtered against resolved credentials. A configured provider, a model catalog entry, and an account entitlement are distinct requirements. Use [authentication](authentication.md) and `rpi doctor` when a model is missing or refused.

`--models PATTERNS` scopes interactive cycling. In the terminal, Ctrl+L opens the selector and Ctrl+P / Ctrl+Shift+P cycle forward/backward. Thinking levels accepted by the command line are `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`; the selected model determines applicable levels.

An active turn keeps its captured provider/model pair. Selecting a model during streaming changes persistent selection for the next turn rather than relabeling the active response.

## Custom provider configuration

The active agent directory contains `models.json` for custom model/provider configuration and `auth.json` for stored credentials. Run `rpi setup` to locate those files. Provider request configuration can include API-key configuration, headers, per-model headers, and authentication-header behavior. Keep credentials out of project source and shared examples.

Model identity is the pair `(provider, model identifier)`, not the identifier alone. Configuration and credential validation errors should be resolved before starting a model turn; do not select a different provider merely because it reuses an identifier.

## Dynamic and local catalogs

`rpi update --models` refreshes dynamic model catalogs. The llama.cpp router can refresh its configured catalog; when unavailable, the command reports the problem and uses its cached catalog where supported. `--offline` suppresses nonessential startup refreshes rather than forbidding all network requests.

See [local models](../reference/local-llama.md) for router management. Imported sessions use a neutral `imported` hint when a synthetic model hint is required; import history is not a provider configuration.
