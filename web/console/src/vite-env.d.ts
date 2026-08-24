/// <reference types="vite/client" />

interface QwenthinDesktop {
  platform: string;
  close: () => void;
  minimize: () => void;
  toggleMaximize: () => void;
}

interface Window {
  qwenthinDesktop?: QwenthinDesktop;
}
