/**
 * Thin dsh plugin: spawn `q38 --sidecar` and forward JSON-RPC.
 * Forbidden here: a second tool loop, Cordis rewrite, MCP.
 */
import { spawn, type ChildProcess } from 'node:child_process';
import { createInterface, type Interface as ReadlineInterface } from 'node:readline';

export const METHODS = ['session.open', 'slash', 'turn.start', 'turn.abort'] as const;

export type JsonRpcId = number | string;

export type SessionEvent = {
  type: string;
  text?: string;
  content?: string;
  reasoning?: string;
  /** `delta`: reasoning | content. `session/start`: inbound surface. */
  channel?: string;
  delta?: boolean;
  reset?: boolean;
  name?: string;
  output?: string;
  reason?: string;
  [key: string]: unknown;
};

export type RpcRequest = {
  jsonrpc: '2.0';
  id: JsonRpcId;
  method: string;
  params?: unknown;
};

export type RpcErrorShape = {
  code: number;
  message: string;
};

export type RpcResponse = {
  jsonrpc: '2.0';
  id: JsonRpcId;
  result?: unknown;
  error?: RpcErrorShape;
};

export type RpcNotification = {
  jsonrpc: '2.0';
  method: 'event.append';
  params: SessionEvent;
};

export type EventHandler = (event: SessionEvent) => void;

export class RpcError extends Error {
  readonly code: number;

  constructor(err: RpcErrorShape) {
    super(err.message);
    this.name = 'RpcError';
    this.code = err.code;
  }
}

export type SpawnOptions = {
  workspace: string;
  session: string;
  /** Binary name or path. Default `q38`. */
  command?: string;
};

type Pending = {
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
};

/**
 * Dumb pipe to `q38 --sidecar`. Does not execute tools.
 */
export class Q38Sidecar {
  private readonly child: ChildProcess;
  private readonly rl: ReadlineInterface;
  private readonly pending = new Map<JsonRpcId, Pending>();
  private readonly listeners = new Set<EventHandler>();
  private nextId = 1;
  private closed = false;

  private constructor(child: ChildProcess) {
    this.child = child;
    const stdout = child.stdout;
    if (!stdout) {
      throw new Error('q38 sidecar requires piped stdout');
    }
    this.rl = createInterface({ input: stdout, crlfDelay: Infinity });
    this.rl.on('line', (line) => this.onLine(line));
    this.rl.on('close', () => this.failAll(new Error('q38 sidecar stdout closed')));
    child.on('error', (err) => this.failAll(err));
    child.on('exit', (code, signal) => {
      if (this.closed) {
        return;
      }
      this.failAll(
        new Error(
          signal
            ? `q38 sidecar killed by ${signal}`
            : `q38 sidecar exited with code ${code ?? 'unknown'}`,
        ),
      );
    });
  }

  static spawn(opts: SpawnOptions): Q38Sidecar {
    const child = spawn(
      opts.command ?? 'q38',
      ['--sidecar', '--workspace', opts.workspace, '--session', opts.session],
      { stdio: ['pipe', 'pipe', 'inherit'] },
    );
    if (!child.stdin || !child.stdout) {
      throw new Error('q38 sidecar requires piped stdin and stdout');
    }
    return new Q38Sidecar(child);
  }

  onEvent(handler: EventHandler): () => void {
    this.listeners.add(handler);
    return () => {
      this.listeners.delete(handler);
    };
  }

  sessionOpen(params: {
    session: string;
    workspace: string;
    mode?: string;
  }): Promise<{ ok: true }> {
    return this.call('session.open', params) as Promise<{ ok: true }>;
  }

  slash(text: string): Promise<{ ok: true }> {
    return this.call('slash', { text }) as Promise<{ ok: true }>;
  }

  turnStart(prompt: string): Promise<{ ok: true }> {
    return this.call('turn.start', { prompt }) as Promise<{ ok: true }>;
  }

  turnAbort(): Promise<{ ok: true }> {
    return this.call('turn.abort', {}) as Promise<{ ok: true }>;
  }

  async close(): Promise<void> {
    if (this.closed) {
      return;
    }
    this.closed = true;
    try {
      await this.turnAbort();
    } catch {
      // process may already be gone
    }
    this.rl.close();
    if (this.child.stdin?.writable) {
      this.child.stdin.end();
    }
    this.child.kill();
    this.failAll(new Error('q38 sidecar closed'));
  }

  call(method: string, params: unknown = {}): Promise<unknown> {
    if (this.closed) {
      return Promise.reject(new Error('q38 sidecar closed'));
    }
    const id = this.nextId++;
    const req: RpcRequest = { jsonrpc: '2.0', id, method, params };
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      const stdin = this.child.stdin;
      if (!stdin) {
        this.pending.delete(id);
        reject(new Error('q38 sidecar stdin is not writable'));
        return;
      }
      const line = `${JSON.stringify(req)}\n`;
      stdin.write(line, (err) => {
        if (err) {
          this.pending.delete(id);
          reject(err);
        }
      });
    });
  }

  private onLine(line: string): void {
    const trimmed = line.trim();
    if (!trimmed) {
      return;
    }
    let msg: RpcResponse & Partial<RpcNotification>;
    try {
      msg = JSON.parse(trimmed) as RpcResponse & Partial<RpcNotification>;
    } catch {
      return;
    }
    if (msg.method === 'event.append' && msg.params && typeof msg.params === 'object') {
      const event = msg.params as SessionEvent;
      for (const handler of this.listeners) {
        handler(event);
      }
      return;
    }
    if (msg.id === undefined || msg.id === null) {
      return;
    }
    const pending = this.pending.get(msg.id);
    if (!pending) {
      return;
    }
    this.pending.delete(msg.id);
    if (msg.error) {
      pending.reject(new RpcError(msg.error));
    } else {
      pending.resolve(msg.result);
    }
  }

  private failAll(err: Error): void {
    for (const pending of this.pending.values()) {
      pending.reject(err);
    }
    this.pending.clear();
  }
}

export function spawnQ38(opts: SpawnOptions): Q38Sidecar {
  return Q38Sidecar.spawn(opts);
}
