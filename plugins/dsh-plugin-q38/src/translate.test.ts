import assert from 'node:assert/strict';
import { describe, it } from 'node:test';

import { injectBootSkin, Q38_INK } from './skin.js';
import { Projector, textOf, userMessage } from './translate.js';

describe('textOf', () => {
  it('reads dsh content blocks', () => {
    assert.equal(
      textOf({ role: 'user', content: [{ type: 'text', text: 'hello' }] }),
      'hello',
    );
  });
  it('reads a plain string', () => {
    assert.equal(textOf('x'), 'x');
  });
});

describe('Projector', () => {
  it('projects a streamed think + answer + tool', () => {
    const p = new Projector();
    p.noteLocalUser();
    assert.deepEqual(p.apply({ type: 'user', text: 'echoed' }), []);

    const deltas = p.apply({
      type: 'delta',
      channel: 'reasoning',
      text: 'plan',
      delta: true,
    });
    assert.equal(deltas[0]?.type, 'turn/start');
    assert.equal(deltas[1]?.type, 'step/start');
    assert.equal(
      (deltas[2]?.data as { chunk: { type: string } }).chunk.type,
      'block-start',
    );
    assert.equal(
      (deltas[3]?.data as { chunk: { type: string } }).chunk.type,
      'reasoning-delta',
    );

    const assistant = p.apply({
      type: 'assistant',
      content: 'done',
      reasoning: 'plan',
      tool_calls: [
        {
          id: 'c1',
          function: { name: 'read', arguments: '{"path":"a.rs"}' },
        },
      ],
    });
    const types = assistant.map((op) => op.type);
    assert.ok(types.includes('tool/call'));
    assert.ok(types.includes('assistant/message'));
    assert.ok(types.includes('step/end'));
    const call = assistant.find((op) => op.type === 'tool/call');
    assert.equal((call?.data as { name: string }).name, 'read');

    const tool = p.apply({
      type: 'tool',
      tool_call_id: 'c1',
      name: 'read',
      output: 'fn main',
    });
    assert.equal(tool[0]?.type, 'tool/result');
    assert.equal(tool[0]?.surface?.surfaceOp, 'append');

    const stop = p.apply({ type: 'stop', reason: 'stop' });
    assert.equal(stop.at(-1)?.type, 'turn/end');
  });

  it('builds a user message the dsh surface can append', () => {
    const msg = userMessage('hi', 'id-1');
    assert.equal(msg.role, 'user');
    assert.equal(msg.source.kind, 'user');
    assert.equal(msg.content[0]?.text, 'hi');
  });
});

describe('skin', () => {
  it('injects a style tag once', () => {
    const html = '<html><head><title>dsh</title></head><body></body></html>';
    const once = injectBootSkin(html);
    assert.match(once, /id="q38-skin"/);
    assert.match(once, new RegExp(Q38_INK['--dsw-alias-brand-primary']!.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
    const twice = injectBootSkin(once);
    assert.equal(twice.split('id="q38-skin"').length, 2);
  });
});
