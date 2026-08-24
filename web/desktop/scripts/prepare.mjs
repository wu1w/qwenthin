import { chmodSync, copyFileSync, existsSync, mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const desktop = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const root = path.resolve(desktop, "../..");
const logo = path.join(root, "web/console/src/assets/logo.png");
const iconOut = path.join(desktop, "build/icon.png");
const macBinSrc = path.join(root, "target/release/q38");
const winCandidates = [
  path.join(root, "target/x86_64-pc-windows-msvc/release/q38.exe"),
  path.join(root, "target/x86_64-pc-windows-gnu/release/q38.exe"),
];

function die(msg) {
  console.error(msg);
  process.exit(1);
}

function copyIcon() {
  mkdirSync(path.join(desktop, "build"), { recursive: true });
  if (process.platform === "darwin") {
    const r = spawnSync(
      "sips",
      ["-z", "1024", "1024", logo, "--out", iconOut],
      { stdio: "inherit" },
    );
    if (r.status === 0 && existsSync(iconOut)) return;
  }
  copyFileSync(logo, iconOut);
}

function stageMac() {
  if (!existsSync(macBinSrc)) {
    die(`缺少 macOS q38: ${macBinSrc}\n先执行 cargo build --release -p q38-cli`);
  }
  const destDir = path.join(desktop, "resources/mac");
  mkdirSync(destDir, { recursive: true });
  const dest = path.join(destDir, "q38");
  copyFileSync(macBinSrc, dest);
  chmodSync(dest, 0o755);
}

function stageWin() {
  const src = winCandidates.find((p) => existsSync(p));
  if (!src) return false;
  const destDir = path.join(desktop, "resources/win");
  mkdirSync(destDir, { recursive: true });
  copyFileSync(src, path.join(destDir, "q38.exe"));
  return true;
}

const requireWin = process.argv.includes("--require-win");
copyIcon();
stageMac();
const win = stageWin();
if (requireWin && !win) {
  die(
    "缺少 Windows q38.exe。先装 cargo-xwin（或 mingw-w64）再跑 scripts/build-sidecars.sh",
  );
}
console.log(win ? "staged mac + win sidecars" : "staged mac sidecar (no Windows q38.exe yet)");
