const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("qwenthinDesktop", {
  platform: process.platform,
  close: () => ipcRenderer.send("desktop:close"),
  minimize: () => ipcRenderer.send("desktop:min"),
  toggleMaximize: () => ipcRenderer.send("desktop:max"),
});
