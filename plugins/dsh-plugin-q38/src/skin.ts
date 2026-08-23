/** q38 alias-token skins. Applied by the dsh ui-theme presenter as body CSS variables. */

export type ColorScheme = 'light' | 'dark';

export type ThemeTokens = Record<string, string>;

export type SkinId = 'q38-ink' | 'q38-paper';

export type SkinDefinition = {
  id: SkinId;
  colorScheme: ColorScheme;
  label: string;
  tokens: ThemeTokens;
};

/** Dark ink: warm charcoal, teal accent. Distinct from DeepSeek green and Grok pager. */
export const Q38_INK: ThemeTokens = {
  '--dsw-alias-bg-base': '#12110e',
  '--dsw-alias-bg-layer-1': '#1a1915',
  '--dsw-alias-bg-layer-2': '#22211c',
  '--dsw-alias-bg-layer-3': '#2b2a24',
  '--dsw-alias-bg-overlay': '#33322b',
  '--dsw-alias-bg-module-platform': '#2b2a24',
  '--dsw-alias-border-l1': 'rgba(245, 240, 230, 0.06)',
  '--dsw-alias-border-l2': 'rgba(245, 240, 230, 0.12)',
  '--dsw-alias-border-l3': 'rgba(245, 240, 230, 0.18)',
  '--dsw-alias-label-primary': '#f4f0e6',
  '--dsw-alias-label-secondary': '#b8b2a4',
  '--dsw-alias-label-tertiary': '#8e887a',
  '--dsw-alias-label-caption': '#8e887a',
  '--dsw-alias-label-dimmed': '#6f6a5e',
  '--dsw-alias-brand-primary': '#5eead4',
  '--dsw-alias-brand-text': '#f4f0e6',
  '--dsw-alias-button-primary-fill': '#5eead4',
  '--dsw-alias-button-primary-hover': '#2dd4bf',
  '--dsw-alias-button-info-fill': '#5eead4',
  '--dsw-alias-button-info-hover': '#2dd4bf',
  '--dsw-alias-state-business-primary': '#5eead4',
  '--dsw-alias-state-business-tertiary': '#1a3a36',
  '--dsw-alias-state-success-primary': '#5eead4',
  '--dsw-alias-state-error-primary': '#f87171',
  '--dsw-alias-state-warn-primary': '#fbbf24',
  '--dsw-specific-sidebar-fill': '#0e0d0b',
  '--dsw-specific-sidebar-nav-item-active': '#2b2a24',
  '--dsw-specific-sidebar-nav-item-hover': '#1a1915',
  '--dsw-specific-sidebar-nav-item-active-accent': '#1f3d38',
  '--dsw-specific-bubble': '#1e1d18',
  '--dsw-specific-bubble-highlight': '#2a2923',
  '--dsw-specific-input-major': '#22211c',
  '--dsw-specific-menu': '#2b2a24',
  '--dsw-specific-selector': '#2b2a24',
  '--dsw-specific-tip': '#22211c',
  '--dsw-alias-markdown-code-block': '#16150f',
  '--dsw-alias-markdown-code-block-banner': '#1a1915',
  '--dsw-alias-markdown-inline-code': '#2b2a24',
  '--dsw-alias-scrollbar-bg-l1': '#3a382f',
  '--dsw-alias-scrollbar-hover-l1': '#4d4a3f',
};

/** Light paper: warm page, same teal. */
export const Q38_PAPER: ThemeTokens = {
  '--dsw-alias-bg-base': '#f4f0e6',
  '--dsw-alias-bg-layer-1': '#faf7f0',
  '--dsw-alias-bg-layer-2': '#eee9dd',
  '--dsw-alias-bg-layer-3': '#e6e0d2',
  '--dsw-alias-bg-overlay': '#fffdf8',
  '--dsw-alias-bg-module-platform': '#eee9dd',
  '--dsw-alias-border-l1': 'rgba(40, 36, 28, 0.08)',
  '--dsw-alias-border-l2': 'rgba(40, 36, 28, 0.14)',
  '--dsw-alias-border-l3': 'rgba(40, 36, 28, 0.2)',
  '--dsw-alias-label-primary': '#1c1a14',
  '--dsw-alias-label-secondary': '#5a5548',
  '--dsw-alias-label-tertiary': '#7a7466',
  '--dsw-alias-label-caption': '#7a7466',
  '--dsw-alias-label-dimmed': '#9a9486',
  '--dsw-alias-brand-primary': '#0f766e',
  '--dsw-alias-brand-text': '#0f766e',
  '--dsw-alias-button-primary-fill': '#0f766e',
  '--dsw-alias-button-primary-hover': '#115e59',
  '--dsw-alias-button-info-fill': '#0f766e',
  '--dsw-alias-button-info-hover': '#115e59',
  '--dsw-alias-state-business-primary': '#0f766e',
  '--dsw-alias-state-business-tertiary': '#cce8e4',
  '--dsw-alias-state-success-primary': '#0f766e',
  '--dsw-alias-state-error-primary': '#dc2626',
  '--dsw-alias-state-warn-primary': '#d97706',
  '--dsw-specific-sidebar-fill': '#ebe5d6',
  '--dsw-specific-sidebar-nav-item-active': '#ddd6c4',
  '--dsw-specific-sidebar-nav-item-hover': '#f3eee4',
  '--dsw-specific-sidebar-nav-item-active-accent': '#b7ddd7',
  '--dsw-specific-bubble': '#efe9db',
  '--dsw-specific-bubble-highlight': '#e4dccb',
  '--dsw-specific-input-major': '#faf7f0',
  '--dsw-specific-menu': '#e6e0d2',
  '--dsw-specific-selector': '#eee9dd',
  '--dsw-specific-tip': '#f3eee4',
  '--dsw-alias-markdown-code-block': '#f7f2e8',
  '--dsw-alias-markdown-code-block-banner': '#eee9dd',
  '--dsw-alias-markdown-inline-code': '#efe9db',
  '--dsw-alias-scrollbar-bg-l1': '#d4cebe',
  '--dsw-alias-scrollbar-hover-l1': '#bbb49f',
};

export const SKINS: readonly SkinDefinition[] = [
  { id: 'q38-ink', colorScheme: 'dark', label: 'q38 ink', tokens: Q38_INK },
  { id: 'q38-paper', colorScheme: 'light', label: 'q38 paper', tokens: Q38_PAPER },
];

export const DEFAULT_SKIN: SkinId = 'q38-ink';

export function cssVars(tokens: ThemeTokens): string {
  return Object.entries(tokens)
    .map(([k, v]) => `${k}: ${v};`)
    .join(' ');
}

/** Boot-time <style> so the first paint is q38 even if the client bundle is late. */
export function bootStyleTag(skin: SkinDefinition = SKINS[0]): string {
  const dark = skin.colorScheme === 'dark' ? ' data-ds-dark-theme="true"' : '';
  return `<style id="q38-skin">${cssVars(skin.tokens)}</style><script>document.documentElement.setAttribute("data-q38-skin","${skin.id}");</script><!-- q38 ${dark} -->`;
}

export function injectBootSkin(html: string, skin: SkinDefinition = SKINS[0]): string {
  const tag = bootStyleTag(skin);
  if (html.includes('id="q38-skin"')) {
    return html;
  }
  if (html.includes('</head>')) {
    return html.replace('</head>', `${tag}</head>`);
  }
  return tag + html;
}
