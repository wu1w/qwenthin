/**
 * Map q38 sidecar SessionEvent → dsh session.append ops.
 * dsh UI only understands user/message, assistant/chunk|message, tool/call|result.
 * q38 JSONL stays the source of truth; this is a projection for the shell.
 */

import type { SessionEvent } from './index.js';

export type SurfaceIntent = {
  surfaceOp: 'append';
  sourceEventSeqs?: number[];
};

export type DshAppend = {
  type: string;
  data: unknown;
  surface?: SurfaceIntent;
};

export type ContentBlock =
  | { type: 'text'; text: string }
  | { type: 'reasoning'; text: string }
  | { type: 'tool-call'; id: string; name: string; arguments: string }
  | { type: 'tool-result'; toolCallId: string; content: Array<{ type: 'text'; text: string }>; isError?: boolean };

export type OpenAiToolCall = {
  id?: string;
  function?: { name?: string; arguments?: unknown };
};

export function newId(): string {
  return globalThis.crypto?.randomUUID?.() ?? `q38-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

export function textOf(message: unknown): string {
  if (typeof message === 'string') {
    return message;
  }
  if (!message || typeof message !== 'object') {
    return '';
  }
  const rec = message as Record<string, unknown>;
  if (typeof rec.text === 'string' && rec.text) {
    return rec.text;
  }
  if (typeof rec.prompt === 'string' && rec.prompt) {
    return rec.prompt;
  }
  const content = rec.content;
  if (typeof content === 'string') {
    return content;
  }
  if (Array.isArray(content)) {
    return content
      .map((block) => {
        if (!block || typeof block !== 'object') {
          return '';
        }
        const b = block as Record<string, unknown>;
        return typeof b.text === 'string' ? b.text : '';
      })
      .filter(Boolean)
      .join('');
  }
  return '';
}

export function userMessage(text: string, id = newId()) {
  return {
    id,
    role: 'user' as const,
    content: [{ type: 'text' as const, text }],
    source: { kind: 'user' as const },
  };
}

export function assistantMessage(opts: {
  id?: string;
  content: ContentBlock[];
  model?: string;
  provider?: string;
}) {
  return {
    id: opts.id ?? newId(),
    role: 'assistant' as const,
    content: opts.content,
    source: {
      kind: 'model' as const,
      provider: opts.provider ?? 'q38',
      model: opts.model ?? 'q38',
    },
  };
}

export function toolResultMessage(opts: {
  callId: string;
  name: string;
  output: string;
  id?: string;
}) {
  return {
    id: opts.id ?? newId(),
    role: 'user' as const,
    content: [
      {
        type: 'tool-result' as const,
        toolCallId: opts.callId,
        content: [{ type: 'text' as const, text: opts.output }],
      },
    ],
    source: { kind: 'tool' as const, toolCallId: opts.callId, name: opts.name },
  };
}

function strArg(value: unknown): string {
  if (typeof value === 'string') {
    return value;
  }
  try {
    return JSON.stringify(value ?? {});
  } catch {
    return '{}';
  }
}

export class Projector {
  turn = 0;
  step = 0;
  inTurn = false;
  inStep = false;
  skipNextUser = false;
  model = 'q38';
  provider = 'q38';
  private reasoning = '';
  private content = '';
  private chunkSeqs: number[] = [];
  /** seq of the last append the caller reported; we only track local counts. */
  private nextLocalSeq = 1;
  private pendingCalls = new Map<string, { name: string; arguments: string }>();

  /** Call after the host appends a user/message from followup, before sidecar echoes it. */
  noteLocalUser(): void {
    this.skipNextUser = true;
  }

  /** Last appended seq if the host reports it; otherwise a local counter. */
  takeSeq(hostSeq?: number): number {
    if (typeof hostSeq === 'number' && Number.isFinite(hostSeq)) {
      this.nextLocalSeq = hostSeq + 1;
      return hostSeq;
    }
    const seq = this.nextLocalSeq;
    this.nextLocalSeq += 1;
    return seq;
  }

  apply(event: SessionEvent): DshAppend[] {
    const type = event.type;
    if (type === 'user') {
      if (this.skipNextUser) {
        this.skipNextUser = false;
        return [];
      }
      const text = typeof event.text === 'string' ? event.text : textOf(event);
      return this.openTurnWithUser(text);
    }
    if (type === 'delta') {
      return this.onDelta(event);
    }
    if (type === 'assistant') {
      return this.onAssistant(event);
    }
    if (type === 'tool') {
      return this.onTool(event);
    }
    if (type === 'stop') {
      return this.onStop();
    }
    if (type === 'policy' || type === 'session/start' || type === 'session/fork' || type === 'session/compact' || type === 'session/undo') {
      return [
        {
          type: `q38/${type}`,
          data: event,
        },
      ];
    }
    return [];
  }

  private ensureTurn(): DshAppend[] {
    const out: DshAppend[] = [];
    if (!this.inTurn) {
      this.turn += 1;
      this.inTurn = true;
      out.push({ type: 'turn/start', data: { turn: this.turn } });
    }
    if (!this.inStep) {
      this.step += 1;
      this.inStep = true;
      this.reasoning = '';
      this.content = '';
      this.chunkSeqs = [];
      out.push({ type: 'step/start', data: { turn: this.turn, step: this.step } });
    }
    return out;
  }

  private openTurnWithUser(text: string): DshAppend[] {
    const out = this.ensureTurn();
    out.push({
      type: 'user/message',
      data: userMessage(text),
      surface: { surfaceOp: 'append' },
    });
    return out;
  }

  private onDelta(event: SessionEvent): DshAppend[] {
    const out = this.ensureTurn();
    const text = typeof event.text === 'string' ? event.text : '';
    if (!text) {
      return out;
    }
    if (event.reset) {
      this.reasoning = '';
      this.content = '';
      this.chunkSeqs = [];
    }
    const channel = event.channel === 'reasoning' ? 'reasoning' : 'content';
    const index = channel === 'reasoning' ? 1 : 0;
    const blockType = channel === 'reasoning' ? 'reasoning' : 'text';
    const started = channel === 'reasoning' ? this.reasoning.length > 0 : this.content.length > 0;
    if (!started) {
      out.push({
        type: 'assistant/chunk',
        data: {
          turn: this.turn,
          step: this.step,
          chunk: { type: 'block-start', index, blockType },
        },
      });
    }
    if (channel === 'reasoning') {
      this.reasoning += text;
      out.push({
        type: 'assistant/chunk',
        data: {
          turn: this.turn,
          step: this.step,
          chunk: { type: 'reasoning-delta', index, text },
        },
      });
    } else {
      this.content += text;
      out.push({
        type: 'assistant/chunk',
        data: {
          turn: this.turn,
          step: this.step,
          chunk: { type: 'text-delta', index, text },
        },
      });
    }
    return out;
  }

  private onAssistant(event: SessionEvent): DshAppend[] {
    const out = this.ensureTurn();
    const contentText = typeof event.content === 'string' ? event.content : '';
    const reasoningText = typeof event.reasoning === 'string' ? event.reasoning : this.reasoning;
    const visible = contentText || this.content;
    const blocks: ContentBlock[] = [];
    if (reasoningText) {
      blocks.push({ type: 'reasoning', text: reasoningText });
      out.push({
        type: 'assistant/chunk',
        data: {
          turn: this.turn,
          step: this.step,
          chunk: {
            type: 'block-end',
            index: 1,
            block: { type: 'reasoning', text: reasoningText },
          },
        },
      });
    }
    if (visible) {
      blocks.push({ type: 'text', text: visible });
      out.push({
        type: 'assistant/chunk',
        data: {
          turn: this.turn,
          step: this.step,
          chunk: {
            type: 'block-end',
            index: 0,
            block: { type: 'text', text: visible },
          },
        },
      });
    }
    const calls = Array.isArray(event.tool_calls) ? (event.tool_calls as OpenAiToolCall[]) : [];
    for (const call of calls) {
      const id = typeof call.id === 'string' && call.id ? call.id : newId();
      const name = call.function?.name ?? 'unknown';
      const args = strArg(call.function?.arguments);
      this.pendingCalls.set(id, { name, arguments: args });
      blocks.push({ type: 'tool-call', id, name, arguments: args });
      out.push({
        type: 'tool/call',
        data: {
          turn: this.turn,
          step: this.step,
          callId: id,
          name,
          arguments: args,
        },
      });
    }
    const finishKind = calls.length > 0 ? 'tool-calls' : 'stop';
    out.push({
      type: 'assistant/chunk',
      data: {
        turn: this.turn,
        step: this.step,
        chunk: { type: 'finish', reason: { kind: finishKind } },
      },
    });
    const promptTokens = typeof event.prompt_tokens === 'number' ? event.prompt_tokens : 0;
    const completionTokens = typeof event.completion_tokens === 'number' ? event.completion_tokens : 0;
    const usage =
      promptTokens || completionTokens
        ? { promptTokens, completionTokens, totalTokens: promptTokens + completionTokens }
        : undefined;
    out.push({
      type: 'assistant/message',
      data: {
        turn: this.turn,
        step: this.step,
        message: assistantMessage({ content: blocks, model: this.model, provider: this.provider }),
        ...(usage ? { usage } : {}),
      },
      surface: { surfaceOp: 'append', sourceEventSeqs: this.chunkSeqs.length ? [...this.chunkSeqs] : [] },
    });
    out.push({ type: 'step/end', data: { turn: this.turn, step: this.step } });
    this.inStep = false;
    this.reasoning = '';
    this.content = '';
    this.chunkSeqs = [];
    return out;
  }

  private onTool(event: SessionEvent): DshAppend[] {
    const out: DshAppend[] = [];
    const callId = typeof event.tool_call_id === 'string' ? event.tool_call_id : newId();
    const name =
      typeof event.name === 'string'
        ? event.name
        : (this.pendingCalls.get(callId)?.name ?? 'tool');
    const output = typeof event.output === 'string' ? event.output : '';
    this.pendingCalls.delete(callId);
    if (!this.inTurn) {
      out.push(...this.ensureTurn());
    }
    out.push({
      type: 'tool/result',
      data: {
        turn: this.turn,
        step: Math.max(this.step, 1),
        message: toolResultMessage({ callId, name, output }),
      },
      surface: { surfaceOp: 'append' },
    });
    return out;
  }

  private onStop(): DshAppend[] {
    const out: DshAppend[] = [];
    if (this.inStep) {
      out.push({ type: 'step/end', data: { turn: this.turn, step: this.step } });
      this.inStep = false;
    }
    if (this.inTurn) {
      out.push({ type: 'turn/end', data: { turn: this.turn, reason: { kind: 'stop' } } });
      this.inTurn = false;
    }
    return out;
  }
}
