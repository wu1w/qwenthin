/**
 * Cordis Agent factory. dsh UI talks to this; tools/ThinkPolicy stay in q38.
 */
import path from 'node:path';

import { Q38Sidecar, type SessionEvent } from './index.js';
import { Projector, textOf, userMessage, type DshAppend } from './translate.js';

export type PluginConfig = {
  command?: string;
};

type DshSession = {
  id?: string;
  append: (type: string, data: unknown, surface?: { surfaceOp: 'append'; sourceEventSeqs?: number[] }) => { seq?: number };
};

type DshCtx = {
  agents: {
    setFactory: (factory: unknown) => () => void;
    register: (agent: unknown) => () => void;
  };
  sessions: {
    create: (id?: string, options?: { seed?: unknown; meta?: Record<string, unknown> }) => DshSession;
    prepare?: (id?: string, options?: unknown) => DshSession;
    enter?: (session: DshSession) => () => void;
    announce?: (session: DshSession) => void;
    get?: (id: string) => DshSession | undefined;
  };
  emit?: (event: string, ...args: unknown[]) => void;
  inject?: (deps: string[], fn: (child: DshCtx) => void) => unknown;
  effect?: (fn: () => (() => void) | void, name?: string) => () => void;
  webServer?: { tapIndex: (fn: (html: string) => string) => () => void };
};

type AgentStatus = { kind: 'idle' } | { kind: 'running' };

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

function absCwd(options: Record<string, unknown>): string {
  const meta = asRecord(options.meta);
  const agentOptions = asRecord(options.agentOptions);
  const raw =
    (typeof meta.cwd === 'string' && meta.cwd) ||
    (typeof agentOptions.cwd === 'string' && agentOptions.cwd) ||
    process.cwd();
  return path.resolve(raw);
}

function sessionIdOf(options: Record<string, unknown>, fallback: string): string {
  const id = options.sessionId ?? options.resumeSessionId ?? options.id;
  return typeof id === 'string' && id ? id : fallback;
}

export class Q38Agent {
  readonly id: string;
  readonly options: Record<string, unknown>;
  readonly session: DshSession;
  readonly ctx: DshCtx;
  status: AgentStatus = { kind: 'idle' };
  private readonly command: string;
  private readonly workspace: string;
  private sidecar?: Q38Sidecar;
  private readonly projector = new Projector();
  private opening?: Promise<void>;
  private unsub?: () => void;

  constructor(opts: {
    id: string;
    ctx: DshCtx;
    session: DshSession;
    workspace: string;
    command: string;
    agentOptions?: Record<string, unknown>;
    model?: string;
  }) {
    this.id = opts.id;
    this.ctx = opts.ctx;
    this.session = opts.session;
    this.workspace = opts.workspace;
    this.command = opts.command;
    this.options = {
      cwd: opts.workspace,
      provider: 'q38',
      model: opts.model ?? 'q38',
      ...(opts.agentOptions ?? {}),
    };
    this.projector.model = String(this.options.model ?? 'q38');
    this.projector.provider = 'q38';
  }

  send(message: unknown, _target: string, wakeup: boolean): void {
    if (!wakeup) {
      return;
    }
    void this.run(message);
  }

  followup(message: unknown): void {
    this.send(message, 'next-turn', true);
  }

  steer(message: unknown): void {
    this.send(message, 'next-step', true);
  }

  inject(_message: unknown): void {
    // q38 injects skills as hidden user after the live query inside the Rust loop.
  }

  cancel(): void {
    void this.sidecar?.turnAbort().catch(() => undefined);
    this.status = { kind: 'idle' };
  }

  async dispose(): Promise<void> {
    this.unsub?.();
    this.unsub = undefined;
    if (this.sidecar) {
      await this.sidecar.close();
      this.sidecar = undefined;
    }
  }

  private async ensureOpen(): Promise<Q38Sidecar> {
    if (this.sidecar) {
      return this.sidecar;
    }
    if (!this.opening) {
      this.opening = this.boot();
    }
    await this.opening;
    if (!this.sidecar) {
      throw new Error('q38 sidecar failed to start');
    }
    return this.sidecar;
  }

  private async boot(): Promise<void> {
    const sidecar = Q38Sidecar.spawn({
      workspace: this.workspace,
      session: this.id,
      command: this.command,
    });
    this.sidecar = sidecar;
    this.unsub = sidecar.onEvent((event) => this.onSidecarEvent(event));
    await sidecar.sessionOpen({
      session: this.id,
      workspace: this.workspace,
      mode: 'agent',
    });
  }

  private onSidecarEvent(event: SessionEvent): void {
    const ops = this.projector.apply(event);
    for (const op of ops) {
      this.commit(op);
    }
    if (event.type === 'stop') {
      this.status = { kind: 'idle' };
    }
  }

  private commit(op: DshAppend): void {
    try {
      const appended = this.session.append(op.type, op.data, op.surface);
      if (op.type === 'assistant/chunk' && appended?.seq != null) {
        this.projector.takeSeq(appended.seq);
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      process.stderr.write(`q38-plugin: session.append ${op.type} failed: ${message}\n`);
    }
  }

  private async run(message: unknown): Promise<void> {
    const text = textOf(message);
    if (!text.trim()) {
      return;
    }
    this.status = { kind: 'running' };
    const alreadyUser =
      message && typeof message === 'object' && (message as { role?: string }).role === 'user';
    const payload = alreadyUser ? message : userMessage(text);
    this.commit({
      type: 'user/message',
      data: payload,
      surface: { surfaceOp: 'append' },
    });
    this.projector.noteLocalUser();
    try {
      const sidecar = await this.ensureOpen();
      if (text.trimStart().startsWith('/')) {
        await sidecar.slash(text);
        this.status = { kind: 'idle' };
        return;
      }
      await sidecar.turnStart(text);
    } catch (err) {
      this.status = { kind: 'idle' };
      const messageText = err instanceof Error ? err.message : String(err);
      process.stderr.write(`q38-plugin: turn failed: ${messageText}\n`);
    }
  }
}

export class Q38Factory {
  constructor(
    private readonly ctx: DshCtx,
    private readonly config: PluginConfig,
  ) {}

  private command(): string {
    return this.config.command || process.env.Q38_BIN || 'q38';
  }

  async createAgent(ownerCtx: DshCtx, options: Record<string, unknown> = {}) {
    return this.spawn(ownerCtx, options, false);
  }

  async resume(ownerCtx: DshCtx, options: Record<string, unknown> = {}) {
    return this.spawn(ownerCtx, options, true);
  }

  private spawn(ownerCtx: DshCtx, options: Record<string, unknown>, resume: boolean) {
    const workspace = absCwd(options);
    const id = sessionIdOf(options, `q38-${Date.now()}`);
    const meta = { cwd: workspace, ...asRecord(options.meta) };
    let session: DshSession;
    if (resume && typeof ownerCtx.sessions.get === 'function') {
      session = ownerCtx.sessions.get(id) ?? ownerCtx.sessions.create(id, { meta, seed: options.seed });
    } else {
      session = ownerCtx.sessions.create(id, { meta, seed: options.seed });
    }
    const agent = new Q38Agent({
      id,
      ctx: ownerCtx,
      session,
      workspace,
      command: this.command(),
      agentOptions: asRecord(options.agentOptions),
    });
    const unregister = ownerCtx.agents.register(agent);
    ownerCtx.emit?.('agent/session-start', { agent, session });
    return {
      agent,
      dispose: async () => {
        await agent.dispose();
        unregister?.();
      },
    };
  }
}
