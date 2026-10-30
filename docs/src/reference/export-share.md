# Export and share

Export creates a local artifact. Share uploads an artifact through GitHub CLI. Import converts a foreign conversation into a native session. These operations have different privacy and persistence effects.

## Local export

```text
rpi export SESSION_PATH --output conversation.html
rpi export SESSION_PATH --output conversation.jsonl --jsonl
rpi --export SESSION_PATH --output conversation.html
```

Standalone export does not require a model, provider authentication, or network access. It prints the output path. HTML export includes the full chronological file record, including compaction markers, with inline assets and escaped transcript content. It is not restricted to the active branch.

JSONL export writes the current root-to-leaf branch as a Pi v3 session suitable for resume. Export rejects writing over the source identity and publishes stored output atomically.

Interactive `/dump [--jsonl] [path]` uses the live session export path. HTML is the ordinary format; `--jsonl` or a `.jsonl` output suffix selects JSONL. With no explicit JSONL destination, the output is placed in the session working directory rather than overwriting the stored journal.

## Share

`/share` uses `gh gist create` with secret-gist visibility. GitHub CLI must be installed and authenticated; credentials remain owned by `gh`. A secret gist is unlisted, **not access-controlled private storage**. Anyone with its URL can read it.

`PI_SHARE_VIEWER_URL` changes the returned viewer URL. If it contains `{url}`, the gist URL is substituted; otherwise the configured value is used directly. Without it, the gist URL itself is returned.

Sharing is refused when the session is offline or `RPI_OFFLINE` is truthy (`1`, `true`, or `yes`, case-insensitive). Local export remains a separate operation.

## Import

`rpi import-session SOURCE INPUT [--output PATH]` accepts `pi`, `omp`, `codex`, `claude`, `grok`, and `droid`. Conversion is delegated to agent-loader; an explicit output must not already exist. Empty/unconvertible content fails.

OMP chain import preserves root-to-leaf order, leaf identity/working directory, root start time, and first-occurrence message-identifier deduplication. Oversize chain members are excluded from conversion while retained as lineage boundaries. See [session recovery](../user-guide/session-recovery.md).

## Safety

Review exported content before upload. HTML escaping prevents transcript markup injection; it does not remove every sensitive fact in a conversation. Redaction is not a substitute for reviewing paths, proprietary code, personal information, and credentials. Keep native journals and recovery sidecars when lossless recovery matters.
