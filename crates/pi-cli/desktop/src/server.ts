import { spawn, type ChildProcess } from 'node:child_process';
import { get } from 'node:http';
import { createServer } from 'node:net';
import { setTimeout as delay } from 'node:timers/promises';

const START_TIMEOUT_MS = 30_000;
const PROBE_TIMEOUT_MS = 750;
const INTERRUPT_TIMEOUT_MS = 5_000;
const KILL_TIMEOUT_MS = 2_000;

function errorCode(error: unknown): string {
  const code = (error as NodeJS.ErrnoException | null)?.code;
  return typeof code === 'string' && /^[A-Z0-9_]{1,40}$/.test(code) ? code : 'UNKNOWN';
}

async function assertPortAvailable(port: number): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const reservation = createServer();
    reservation.once('error', (error) => reject(new Error(
      `Cannot start desktop runner on port ${port}: ${errorCode(error)} (port unavailable; no existing process was adopted).`,
    )));
    reservation.listen({ host: '127.0.0.1', port, exclusive: true }, () => {
      reservation.close((error) => error
        ? reject(new Error(`Cannot release desktop port reservation: ${errorCode(error)}.`))
        : resolve());
    });
  });
}

function probeStatus(origin: string, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const request = get(`${origin}/status`, { signal, agent: false }, (response) => {
      if (response.statusCode !== 200) {
        response.destroy();
        reject(new Error(`Runner readiness returned HTTP ${response.statusCode ?? 0}.`));
        return;
      }
      let body = '';
      response.setEncoding('utf8');
      response.on('data', (chunk: string) => {
        body += chunk;
        if (body.length > 4096) {
          response.destroy();
          reject(new Error('Runner readiness response exceeded 4096 characters.'));
        }
      });
      response.once('error', () => reject(new Error('Runner readiness response was interrupted.')));
      response.once('end', () => {
        try {
          const value: unknown = JSON.parse(body);
          if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error();
          const status = value as Record<string, unknown>;
          if (Object.keys(status).sort().join(',') !== 'codename,tokenRequired,version'
            || status.tokenRequired !== false
            || typeof status.version !== 'string' || !status.version.trim()
            || (status.codename !== null
              && (typeof status.codename !== 'string' || !status.codename.trim()))) throw new Error();
          resolve();
        } catch {
          reject(new Error('Runner readiness returned an incompatible /status response.'));
        }
      });
    });
    const timer = setTimeout(() => request.destroy(new Error('Runner readiness request timed out.')), PROBE_TIMEOUT_MS);
    request.once('close', () => clearTimeout(timer));
    request.once('error', (error) => reject(error));
  });
}

export class DesktopServer {
  private readonly options: { binaryPath: string; cwd: string; port: number };
  private child?: ChildProcess;
  private childDone?: Promise<void>;
  private childFailure?: Error;
  private starting?: Promise<string>;
  private stopping?: Promise<void>;
  private terminating?: Promise<void>;
  private controller?: AbortController;
  private ready = false;

  constructor(options: { binaryPath: string; cwd: string; port: number }) {
    if (!Number.isInteger(options.port) || options.port < 1 || options.port > 65535) {
      throw new Error('Desktop runner port must be an integer from 1 to 65535.');
    }
    if (!options.binaryPath || !options.cwd) throw new Error('Desktop runner requires an executable and working directory.');
    this.options = { ...options };
  }

  get ownsProcess(): boolean {
    return this.child !== undefined;
  }

  start(): Promise<string> {
    if (this.stopping) return Promise.reject(new Error('Desktop runner is stopping.'));
    if (this.starting) return this.starting;
    const origin = `http://127.0.0.1:${this.options.port}`;
    if (this.ready && this.child) return Promise.resolve(origin);
    if (this.child) return Promise.reject(new Error('Previous desktop runner has not exited.'));
    const controller = new AbortController();
    this.controller = controller;
    const starting = this.launch(origin, controller.signal);
    this.starting = starting;
    const clear = () => { if (this.starting === starting) this.starting = undefined; };
    void starting.then(clear, clear);
    return starting;
  }

  stop(): Promise<void> {
    if (this.stopping) return this.stopping;
    this.controller?.abort();
    this.ready = false;
    const stopping = (async () => {
      // launch owns cleanup while starting; wait before considering a subsequent child.
      if (this.starting) await this.starting.catch(() => undefined);
      await this.terminate();
    })();
    this.stopping = stopping;
    const clear = () => { if (this.stopping === stopping) this.stopping = undefined; };
    void stopping.then(clear, clear);
    return stopping;
  }

  private async launch(origin: string, signal: AbortSignal): Promise<string> {
    try {
      await assertPortAvailable(this.options.port);
      signal.throwIfAborted();
      this.childFailure = undefined;
      const child = spawn(this.options.binaryPath, [
        '--listen', `127.0.0.1:${this.options.port}`, '--listen-plaintext',
      ], { cwd: this.options.cwd, shell: false, stdio: 'ignore' });
      this.child = child;
      this.childDone = new Promise((resolve) => {
        const finish = () => {
          if (this.child === child) { this.child = undefined; this.ready = false; }
          resolve();
        };
        child.once('error', (error) => {
          this.childFailure = new Error(`Desktop runner process error: ${errorCode(error)}.`);
          if (child.pid === undefined) finish();
        });
        child.once('exit', (code, exitSignal) => {
          this.childFailure = new Error(`Desktop runner exited (code ${code ?? 'none'}, signal ${exitSignal ?? 'none'}).`);
          finish();
        });
      });
      const deadline = Date.now() + START_TIMEOUT_MS;
      while (Date.now() < deadline) {
        signal.throwIfAborted();
        if (this.childFailure) throw this.childFailure;
        try {
          await probeStatus(origin, signal);
        } catch (error) {
          signal.throwIfAborted();
          if (this.childFailure) throw this.childFailure;
          // Only connection refusal means the owned child is still binding.
          if (errorCode(error) !== 'ECONNREFUSED') throw error;
          await delay(100, undefined, { signal });
          continue;
        }
        signal.throwIfAborted();
        if (this.childFailure) throw this.childFailure;
        if (this.child !== child) throw new Error('Desktop runner exited before readiness.');
        this.ready = true;
        return origin;
      }
      throw new Error(`Desktop runner readiness timed out after ${START_TIMEOUT_MS / 1000} seconds.`);
    } catch (error) {
      await this.terminate();
      if (signal.aborted) throw new Error('Desktop runner startup was cancelled.');
      // Never echo paths, response bodies, environment, or runner output.
      if (error instanceof Error && error.message.startsWith('Runner readiness')) throw error;
      if (error instanceof Error && /^(Desktop runner|Cannot (start|release) desktop)/.test(error.message)) throw error;
      throw new Error(`Desktop runner startup failed: ${errorCode(error)}.`);
    }
  }

  private terminate(): Promise<void> {
    if (this.terminating) return this.terminating;
    const child = this.child;
    const done = this.childDone;
    if (!child || !done) return Promise.resolve();
    const terminating = (async () => {
      child.kill('SIGINT');
      if (await this.waitForExit(done, INTERRUPT_TIMEOUT_MS)) return;
      if (this.child === child) child.kill('SIGKILL');
      if (!await this.waitForExit(done, KILL_TIMEOUT_MS)) {
        throw new Error('Desktop runner did not exit after SIGINT and SIGKILL.');
      }
    })();
    this.terminating = terminating;
    const clear = () => { if (this.terminating === terminating) this.terminating = undefined; };
    void terminating.then(clear, clear);
    return terminating;
  }

  private async waitForExit(done: Promise<void>, timeout: number): Promise<boolean> {
    let timer: NodeJS.Timeout | undefined;
    try {
      return await Promise.race([
        done.then(() => true),
        new Promise<false>((resolve) => { timer = setTimeout(() => resolve(false), timeout); }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  }
}
