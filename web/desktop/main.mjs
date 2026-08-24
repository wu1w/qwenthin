import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { app, BrowserWindow, Menu, dialog, shell, ipcMain } from "electron";

const here = path.dirname(fileURLToPath(import.meta.url));
const READY_RE = /q38 web\s+(https?:\/\/\S+)/i;
const READY_MS = 20_000;

let backend = null;
let backendUrl = "";
let mainWindow = null;
let quitting = false;

function repoRoot() {
  return path.resolve(here, "../..");
}

function sidecarPaths() {
  if (app.isPackaged) {
    const binName = process.platform === "win32" ? "q38.exe" : "q38";
    return {
      bin: path.join(process.resourcesPath, "bin", binName),
      consoleDir: path.join(process.resourcesPath, "console"),
    };
  }
  const root = repoRoot();
  const binName = process.platform === "win32" ? "q38.exe" : "q38";
  const release = path.join(root, "target", "release", binName);
  const debug = path.join(root, "target", "debug", binName);
  return {
    bin: existsSync(release) ? release : debug,
    consoleDir: path.join(root, "web", "console", "dist"),
  };
}

function sidecarEnv(consoleDir) {
  const env = { ...process.env, Q38_CONSOLE_DIR: consoleDir };
  const key = process.platform === "win32" ? "Path" : "PATH";
  const delim = path.delimiter;
  const home = app.getPath("home");
  const extras = (
    process.platform === "win32"
      ? [
          path.join(home, ".cargo", "bin"),
          path.join(home, ".local", "bin"),
          path.join(home, "go", "bin"),
          path.join(home, "scoop", "shims"),
          path.join(home, "AppData", "Roaming", "npm"),
          process.env.LOCALAPPDATA
            ? path.join(process.env.LOCALAPPDATA, "Microsoft", "WinGet", "Links")
            : "",
          process.env.ProgramFiles
            ? path.join(process.env.ProgramFiles, "Git", "cmd")
            : "C:\\Program Files\\Git\\cmd",
          process.env.ProgramFiles
            ? path.join(process.env.ProgramFiles, "nodejs")
            : "C:\\Program Files\\nodejs",
          process.env.ProgramData
            ? path.join(process.env.ProgramData, "chocolatey", "bin")
            : "C:\\ProgramData\\chocolatey\\bin",
        ]
      : [
          path.join(home, ".cargo", "bin"),
          path.join(home, ".local", "bin"),
          ...(process.platform === "darwin"
            ? ["/opt/homebrew/bin", "/usr/local/bin"]
            : ["/usr/local/bin"]),
          "/usr/bin",
          "/bin",
        ]
  ).filter((dir) => dir && existsSync(dir));
  const current = env.PATH || env.Path || "";
  const parts = current.split(delim).filter(Boolean);
  const prepend = extras.filter((dir) => !parts.includes(dir));
  env[key] = [...prepend, ...parts].join(delim);
  return env;
}

function waitForUrl(child) {
  return new Promise((resolve, reject) => {
    let buf = "";
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error(`q38 web 在 ${READY_MS / 1000}s 内没有打出监听地址`));
    }, READY_MS);

    const onData = (chunk) => {
      buf += chunk.toString("utf8");
      const m = buf.match(READY_RE);
      if (!m) return;
      cleanup();
      resolve(m[1].replace(/\/?$/, "/"));
    };
    const onExit = (code, signal) => {
      cleanup();
      reject(new Error(`q38 web 提前退出 (code=${code} signal=${signal})\n${buf.trim()}`));
    };
    const cleanup = () => {
      clearTimeout(timer);
      child.stdout?.off("data", onData);
      child.stderr?.off("data", onData);
      child.off("error", onError);
      child.off("exit", onExit);
    };
    const onError = (err) => {
      cleanup();
      reject(err);
    };

    child.stdout?.on("data", onData);
    child.stderr?.on("data", onData);
    child.once("error", onError);
    child.once("exit", onExit);
  });
}

async function startBackend() {
  if (backendUrl) return backendUrl;
  const { bin, consoleDir } = sidecarPaths();
  if (!existsSync(bin)) {
    throw new Error(`找不到 q38 可执行文件: ${bin}`);
  }
  if (!existsSync(path.join(consoleDir, "index.html"))) {
    throw new Error(`找不到控制台 dist: ${consoleDir}`);
  }

  const child = spawn(bin, ["web", "--no-open", "--bind", "127.0.0.1:0"], {
    cwd: app.getPath("home"),
    env: sidecarEnv(consoleDir),
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  backend = child;
  backendUrl = await waitForUrl(child);
  child.on("exit", () => {
    if (!quitting) {
      backend = null;
      backendUrl = "";
    }
  });
  return backendUrl;
}

function stopBackend() {
  if (!backend || backend.killed) return;
  const child = backend;
  backend = null;
  backendUrl = "";
  if (process.platform === "win32") {
    spawn("taskkill", ["/pid", String(child.pid), "/t", "/f"], {
      stdio: "ignore",
      windowsHide: true,
    });
    return;
  }
  child.kill("SIGINT");
  setTimeout(() => {
    if (!child.killed) child.kill("SIGKILL");
  }, 1500);
}

function installMenu() {
  const isMac = process.platform === "darwin";
  const template = [
    ...(isMac
      ? [
          {
            role: "appMenu",
          },
        ]
      : [
          {
            label: "文件",
            submenu: [{ role: "quit", label: "退出" }],
          },
        ]),
    { role: "editMenu" },
    { role: "viewMenu" },
    { role: "windowMenu" },
  ];
  Menu.setApplicationMenu(Menu.buildFromTemplate(template));
}

async function createWindow() {
  let url;
  try {
    url = await startBackend();
  } catch (err) {
    dialog.showErrorBox("Qwenthin 无法启动", err instanceof Error ? err.message : String(err));
    app.quit();
    return;
  }

  mainWindow = new BrowserWindow({
    width: 1280,
    height: 840,
    minWidth: 880,
    minHeight: 560,
    title: "Qwenthin",
    backgroundColor: "#eceff4",
    show: false,
    frame: false,
    autoHideMenuBar: true,
    webPreferences: {
      preload: path.join(here, "preload.cjs"),
      sandbox: true,
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  mainWindow.once("ready-to-show", () => mainWindow?.show());
  mainWindow.on("closed", () => {
    mainWindow = null;
  });
  mainWindow.webContents.setWindowOpenHandler(({ url: target }) => {
    shell.openExternal(target);
    return { action: "deny" };
  });
  mainWindow.webContents.on("will-navigate", (event, target) => {
    if (target.startsWith(url)) return;
    event.preventDefault();
    shell.openExternal(target);
  });
  await mainWindow.loadURL(url);
}

const gotLock = app.requestSingleInstanceLock();
if (!gotLock) {
  app.quit();
} else {
  app.on("second-instance", () => {
    if (!mainWindow) return;
    if (mainWindow.isMinimized()) mainWindow.restore();
    mainWindow.focus();
  });

  app.whenReady().then(async () => {
    app.setName("Qwenthin");
    installMenu();
    await createWindow();
    app.on("activate", async () => {
      if (BrowserWindow.getAllWindows().length === 0) await createWindow();
    });
  });

  app.on("window-all-closed", () => {
    if (process.platform !== "darwin") app.quit();
  });

  app.on("before-quit", () => {
    quitting = true;
    stopBackend();
  });
}

ipcMain.on("desktop:close", (event) => {
  BrowserWindow.fromWebContents(event.sender)?.close();
});
ipcMain.on("desktop:min", (event) => {
  BrowserWindow.fromWebContents(event.sender)?.minimize();
});
ipcMain.on("desktop:max", (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win) return;
  if (win.isMaximized()) win.unmaximize();
  else win.maximize();
});
