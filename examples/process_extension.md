# Process extension example

Process extensions are implemented in this release. An extension is a
subprocess that communicates with `rpi` over newline-delimited JSON on stdin
and stdout.

A runnable Rust sample is at [`src/bin/process_extension.rs`](src/bin/process_extension.rs).
It builds an in-process extension runtime, launches the same executable as a
child process, and exercises tools, commands, UI requests, event hooks, trust
gating, and invalidation.

Run it from the repo root:

```sh
cd examples
cargo run --bin process_extension
```

## Package layout

```text
pi-weather/
├── package.json
└── extensions/
    └── pi-weather/
        ├── pi-extension.json
        └── main
```

`package.json`:

```json
{
  "name": "pi-weather",
  "pi": {
    "schemaVersion": 1,
    "extensions": ["extensions/pi-weather/pi-extension.json"]
  }
}
```

`extensions/pi-weather/pi-extension.json`:

```json
{
  "schemaVersion": 1,
  "id": "pi-weather",
  "runtime": "process",
  "executable": "./main",
  "arguments": [],
  "capabilities": ["tools"],
  "uiCapabilities": []
}
```

## Install the package

```sh
rpi install local:./pi-weather
```

## Minimal extension (Node.js sketch)

```js
#!/usr/bin/env node
const readline = require('readline');

const rl = readline.createInterface({ input: process.stdin });

function send(frame) {
  console.log(JSON.stringify(frame));
}

rl.on('line', (line) => {
  const frame = JSON.parse(line);
  if (frame.type === 'hello') {
    send({
      type: 'hello',
      protocolVersion: 1,
      manifest: {
        id: 'pi-weather',
        name: 'Weather',
        version: '0.1.0',
        capabilities: ['tools'],
        uiCapabilities: []
      }
    });
    send({
      type: 'register',
      registration: {
        kind: 'tool',
        tool: {
          name: 'weather',
          label: 'Weather',
          description: 'Get the current weather for a city',
          parameters: {
            type: 'object',
            properties: { city: { type: 'string' } },
            required: ['city']
          }
        }
      }
    });
  } else if (frame.type === 'request' && frame.request.kind === 'invoke') {
    const inv = frame.request.invocation;
    if (inv.kind === 'tool' && inv.name === 'weather') {
      send({
        type: 'response',
        id: frame.id,
        result: {
          status: 'success',
          value: {
            content: [{ type: 'text', text: `Weather in ${inv.arguments.city}: 15 °C, cloudy.` }]
          }
        }
      });
    }
  }
});
```

Make `main` executable and ensure it stays inside the extension directory.

## Capabilities

Valid `capabilities`: `commands`, `tools`, `event_hooks`, `message_renderers`,
`provider_metadata`, `session_actions`, `ui`. Add `uiCapabilities` when using
`ui`.

See [`docs/extensions.md`](../docs/extensions.md) for the full protocol,
request/response shapes, timeouts, and security rules.
