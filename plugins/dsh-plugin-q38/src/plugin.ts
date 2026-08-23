/**
 * Cordis entry: replace dsh agent-loop with q38 sidecar, paint q38 skin.
 * No second tool loop. No MCP. No Cordis rewrite beyond setFactory.
 */
import { Q38Factory, type PluginConfig } from './agent.js';
import { injectBootSkin, SKINS } from './skin.js';

export const name = 'q38-loop';
export const inject = ['agents', 'sessions'];

export * from './index.js';
export * from './translate.js';
export * from './skin.js';

type Ctx = {
  agents: { setFactory: (factory: unknown) => () => void };
  sessions: unknown;
  effect: (fn: () => (() => void) | void, name?: string) => () => void;
  inject: (deps: string[], fn: (child: Ctx) => void) => unknown;
  webServer?: { tapIndex: (fn: (html: string) => string) => () => void };
};

export function apply(ctx: Ctx, config: PluginConfig = {}): void {
  const factory = new Q38Factory(ctx as never, config);
  ctx.effect(() => ctx.agents.setFactory(factory), 'q38-loop.setFactory()');
  if (typeof ctx.inject === 'function') {
    ctx.inject(['webServer'], (http) => {
      http.effect(
        () => http.webServer?.tapIndex((html) => injectBootSkin(html, SKINS[0])),
        'q38-loop.bootSkin',
      );
    });
  }
}
