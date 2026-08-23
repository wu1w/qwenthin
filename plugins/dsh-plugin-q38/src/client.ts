/**
 * Browser half. dsh serves this as the web client bundle.
 * Registers q38-ink / q38-paper on ctx.theme and selects ink.
 */
import { DEFAULT_SKIN, SKINS } from './skin.js';

export const inject = ['theme'];

type ThemeCtx = {
  theme: {
    register: (def: { id: string; colorScheme: 'light' | 'dark'; tokens: Record<string, string> }) => void;
    setTheme: (id: string) => void;
  };
};

export function apply(ctx: ThemeCtx): void {
  for (const skin of SKINS) {
    ctx.theme.register({
      id: skin.id,
      colorScheme: skin.colorScheme,
      tokens: skin.tokens,
    });
  }
  ctx.theme.setTheme(DEFAULT_SKIN);
}
