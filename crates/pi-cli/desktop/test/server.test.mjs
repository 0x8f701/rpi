#!/usr/bin/env node
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';

const packageDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const output = path.join(packageDir, 'dist/server.js');
assert.ok(fs.existsSync(output), 'Build desktop before running server tests.');
assert.ok(fs.statSync(output).mtimeMs >= Math.max(
  fs.statSync(path.join(packageDir, 'src/server.ts')).mtimeMs,
  fs.statSync(path.join(packageDir, 'tsconfig.json')).mtimeMs,
), 'Desktop server output is stale; build desktop first.');
const { DesktopServer } = createRequire(import.meta.url)(output);

async function unusedPort() {
  const server = createServer();
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const port = server.address().port;
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  return port;
}

async function fixture(t, mode = 'ready') {
  const cwd = fs.mkdtempSync(path.join(os.tmpdir(), 'rpi-owned-server-'));
  const binaryPath = path.join(cwd, 'runner');
  fs.writeFileSync(binaryPath, `#!${process.execPath}
const fs = require('node:fs');
const http = require('node:http');
const mode = ${JSON.stringify(mode)};
fs.writeFileSync('started.json', JSON.stringify({ args: process.argv.slice(2), cwd: process.cwd(), pid: process.pid }));
if (mode === 'exit') {
  process.stderr.write('SECRET_TEST_TOKEN=do-not-report\\n');
  process.exit(23);
}
process.on('SIGINT', () => {
  if (mode === 'ignore') return;
  fs.writeFileSync('interrupted', 'SIGINT');
  if (server) server.close(() => process.exit(0));
  else process.exit(0);
});
let server;
if (mode === 'silent') setInterval(() => {}, 1000);
else {
  const port = Number(process.argv[3].split(':').pop());
  server = http.createServer((req, res) => {
    if (mode === 'hang') return;
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify(mode === 'invalid' ? { version: 'fake' } : { tokenRequired: false, version: 'test', codename: mode === 'null-codename' ? null : 'fixture' }));
    if (mode === 'abrupt') setTimeout(() => process.exit(17), 100);
  });
  setTimeout(() => server.listen(port, '127.0.0.1'), 75);
}
`, { mode: 0o700 });
  const port = await unusedPort();
  const server = new DesktopServer({ binaryPath, cwd, port });
  t.after(async () => {
    await server.stop();
    fs.rmSync(cwd, { recursive: true, force: true });
  });
  return { server, cwd, port };
}

test('runner with no assigned codename becomes ready', async (t) => {
  const { server, port } = await fixture(t, 'null-codename');
  assert.equal(await server.start(), `http://127.0.0.1:${port}`);
  assert.equal(server.ownsProcess, true);
  await server.stop();
  assert.equal(server.ownsProcess, false);
});

// Fixtures use executable shebangs; the packaged desktop target is Linux.
test('owned runner becomes ready, reuses its child, and receives terminal SIGINT', async (t) => {
  const { server, cwd, port } = await fixture(t);
  const first = server.start();
  assert.equal(server.start(), first);
  assert.equal(await first, `http://127.0.0.1:${port}`);
  assert.equal(server.ownsProcess, true);
  const started = JSON.parse(fs.readFileSync(path.join(cwd, 'started.json'), 'utf8'));
  assert.deepEqual(started.args, ['--listen', `127.0.0.1:${port}`, '--listen-plaintext']);
  assert.equal(started.cwd, cwd);
  assert.equal(await server.start(), `http://127.0.0.1:${port}`);
  const stopping = server.stop();
  assert.equal(server.stop(), stopping);
  await stopping;
  assert.equal(server.ownsProcess, false);
  assert.equal(fs.readFileSync(path.join(cwd, 'interrupted'), 'utf8'), 'SIGINT');
  assert.throws(() => process.kill(started.pid, 0), { code: 'ESRCH' });
  await server.stop();
  await server.start();
  assert.notEqual(JSON.parse(fs.readFileSync(path.join(cwd, 'started.json'), 'utf8')).pid, started.pid);
});

test('occupied port is neither adopted nor disturbed', async (t) => {
  const { server, cwd, port } = await fixture(t);
  const foreign = createServer((_request, response) => response.end('unrelated'));
  await new Promise((resolve) => foreign.listen(port, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => foreign.close(resolve)));
  await assert.rejects(server.start(), /EADDRINUSE.*no existing process was adopted/);
  assert.equal(fs.existsSync(path.join(cwd, 'started.json')), false);
  await server.stop();
  assert.equal(await (await fetch(`http://127.0.0.1:${port}`)).text(), 'unrelated');
  assert.equal(server.ownsProcess, false);
});

test('missing executable reports safe spawn diagnostics', async (t) => {
  const { cwd, port } = await fixture(t);
  const server = new DesktopServer({ binaryPath: path.join(cwd, 'missing'), cwd, port });
  await assert.rejects(server.start(), /process error: ENOENT/);
  assert.equal(server.ownsProcess, false);
  await server.stop();
});

test('early exit reports code but never child output', async (t) => {
  const { server } = await fixture(t, 'exit');
  await assert.rejects(server.start(), (error) => {
    assert.match(error.message, /code 23/);
    assert.doesNotMatch(error.message, /SECRET|do-not-report/);
    return true;
  });
  assert.equal(server.ownsProcess, false);
});

test('incompatible status is fatal and cleans up owned child', async (t) => {
  const { server, cwd } = await fixture(t, 'invalid');
  await assert.rejects(server.start(), /incompatible \/status/);
  assert.equal(server.ownsProcess, false);
  assert.equal(fs.readFileSync(path.join(cwd, 'interrupted'), 'utf8'), 'SIGINT');
});

test('a listener that never answers has a bounded request deadline', async (t) => {
  const { server } = await fixture(t, 'hang');
  await assert.rejects(server.start(), /readiness request timed out/);
  assert.equal(server.ownsProcess, false);
});

test('a child that never listens has a bounded startup deadline', { timeout: 40_000 }, async (t) => {
  const { server } = await fixture(t, 'silent');
  await assert.rejects(server.start(), /readiness timed out after 30 seconds/);
  assert.equal(server.ownsProcess, false);
});

test('stop cancels a start before spawn and permits a later start', async (t) => {
  const { server, cwd } = await fixture(t);
  const starting = server.start();
  const rejection = assert.rejects(starting, /startup was cancelled/);
  const stopping = server.stop();
  await assert.rejects(server.start(), /is stopping/);
  await Promise.all([rejection, stopping]);
  assert.equal(fs.existsSync(path.join(cwd, 'started.json')), false);
  assert.equal(server.ownsProcess, false);
  await server.start();
  assert.equal(server.ownsProcess, true);
});

test('SIGINT-resistant owned child is killed after grace period', { timeout: 15_000 }, async (t) => {
  const { server, cwd } = await fixture(t, 'ignore');
  await server.start();
  const { pid } = JSON.parse(fs.readFileSync(path.join(cwd, 'started.json'), 'utf8'));
  await server.stop();
  assert.equal(server.ownsProcess, false);
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
});

test('post-readiness abrupt exit clears ownership', async (t) => {
  const { server } = await fixture(t, 'abrupt');
  await server.start();
  const deadline = Date.now() + 3000;
  while (server.ownsProcess && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.equal(server.ownsProcess, false);
  await server.stop();
});

test('stop interrupts an owned child while readiness is pending', async (t) => {
  const { server, cwd } = await fixture(t, 'silent');
  const starting = server.start();
  const rejection = assert.rejects(starting, /startup was cancelled/);
  const deadline = Date.now() + 3000;
  while (!fs.existsSync(path.join(cwd, 'started.json')) && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.equal(fs.existsSync(path.join(cwd, 'started.json')), true);
  await server.stop();
  await rejection;
  assert.equal(server.ownsProcess, false);
});
