# Authentication

Provider credentials and listener tokens serve different purposes. Provider credentials authorize model requests. A Web listener token authorizes control of the running agent.

## Provider setup

```sh
rpi login
rpi models
rpi doctor
```

`rpi login [PROVIDER]` configures an API key or provider subscription. `rpi logout [PROVIDER]` removes the selected stored credential. Provider selection can be interactive when omitted. Use `rpi setup` to locate the active configuration files; do not print credential-file contents into a transcript.

Stored credentials live in `auth.json` under the active agent directory. API-key records can contain a key and provider environment values. Subscription records contain access/refresh credentials and expiry metadata. Diagnostic representations expose credential type and presence, not the secret value.

The model request resolver combines the selected provider/model configuration with applicable credentials and checks whether that credential allows the model. A model identifier in the catalog does not guarantee access under the current account.

## Credential scopes

```text
rpi login PROVIDER --scope work
rpi logout PROVIDER --scope work
```

`PI_AUTH_SCOPE` selects the active scope and takes precedence over the `authScope` setting. A matching scoped credential wins; otherwise the unscoped credential is selected if present. Without an active scope, the unscoped credential is selected. If credentials exist but none is selectable, rpi reports the available scopes without disclosing their values.

Scope labels are trimmed, nonempty, at most 128 bytes, and cannot contain control characters or path separators. Profiles relocate the whole configuration base; scopes select a credential within that base. See [configuration profiles](../reference/configuration-profiles.md).

## Per-run credentials

`--api-key` provides a per-run model credential override. Avoid placing secrets in shell history or process arguments; prefer the login flow or your provider's environment-based configuration. `models.json` can define provider request configuration, but it is not a reason to commit credentials with a project.

## Web pairing

Use `--listen-token-file` for a token-gated listener. The startup pairing URL contains the token and is sensitive. The Web pairing card appears when a listener requires a token and none is stored. It scopes the saved token to that listener and reconnects after a nonempty submission.

Remote listener policy, TLS, and the explicit insecure opt-in are documented in [Web](web.md). Protect token files and do not post pairing URLs in issues, logs, or screenshots.
