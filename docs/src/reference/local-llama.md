# Local / self-hosted models with llama.cpp

rpi manages a configured llama.cpp router and local GGUF model downloads. Router management, model download, and selecting an inference model are separate operations.

## Router commands

```text
rpi llama configure URL
rpi llama status
rpi llama status --reload
rpi llama refresh
rpi llama load MODEL
rpi llama unload MODEL
```

Configure takes the router base URL **without `/v1`**, with optional `--api-key` authentication. Avoid putting real credentials in shell history. Status displays the router's management catalog; `--reload` requests a model-directory rescan. Refresh obtains live inference models and can use the persisted catalog when the router is unavailable.

After configuration, use `rpi models` to see available inference identifiers and select one through `--model` or the interactive model selector. A downloaded file does not by itself establish router availability.

## Download commands

```text
rpi llama search QUERY
rpi llama details OWNER/REPOSITORY
rpi llama download OWNER/REPOSITORY --quantization Q4_K_M
rpi llama installed
```

Search finds GGUF repositories on Hugging Face. Details lists quantizations and checksums. Download supports atomic publication and resume; omitting the quantization selects the recommended or first available choice. Installed lists local downloads.

`rpi update --models` refreshes dynamic model catalogs. `--offline` suppresses nonessential startup refreshes, not an authorization to contact a configured endpoint through an unrelated tool.

## Safety and troubleshooting

Use a trusted router endpoint and protect any bearer credential. Verify model storage and available memory separately from HTTP reachability. If a refresh reports a cached catalog, do not interpret the catalog entry as proof that the router currently serves that model.

See [models](../user-guide/models.md), [authentication](../user-guide/authentication.md), and [sandbox confinement](sandbox-isolation.md).
