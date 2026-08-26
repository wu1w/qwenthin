import { chmodSync, copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const desktop = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const root = path.resolve(desktop, "../..");
const logo = path.join(root, "web/console/src/assets/logo.png");
const buildDir = path.join(desktop, "build");
const iconOut = path.join(buildDir, "icon.png");
const icoOut = path.join(buildDir, "icon.ico");
const icnsOut = path.join(buildDir, "icon.icns");
const macBinSrc = path.join(root, "target/release/q38");
const winCandidates = [
  path.join(root, "target/x86_64-pc-windows-msvc/release/q38.exe"),
  path.join(root, "target/x86_64-pc-windows-gnu/release/q38.exe"),
];

function die(msg) {
  console.error(msg);
  process.exit(1);
}

function wrapPngAsIco(png) {
  const header = Buffer.alloc(22);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(1, 4);
  header.writeUInt8(0, 6);
  header.writeUInt8(0, 7);
  header.writeUInt8(0, 8);
  header.writeUInt8(0, 9);
  header.writeUInt16LE(1, 10);
  header.writeUInt16LE(32, 12);
  header.writeUInt32LE(png.length, 14);
  header.writeUInt32LE(22, 18);
  return Buffer.concat([header, png]);
}

function sipsResize(src, dest, size) {
  const r = spawnSync("sips", ["-z", String(size), String(size), src, "--out", dest], {
    stdio: "ignore",
  });
  return r.status === 0 && existsSync(dest);
}

function copyIcon() {
  mkdirSync(buildDir, { recursive: true });
  if (process.platform === "darwin") {
    if (!sipsResize(logo, iconOut, 1024)) copyFileSync(logo, iconOut);
  } else {
    copyFileSync(logo, iconOut);
  }

  const png256 = path.join(buildDir, "icon-256.png");
  if (!(process.platform === "darwin" && sipsResize(iconOut, png256, 256))) {
    copyFileSync(iconOut, png256);
  }
  writeFileSync(icoOut, wrapPngAsIco(readFileSync(png256)));

  if (process.platform === "darwin") {
    const iconset = path.join(buildDir, "icon.iconset");
    rmSync(iconset, { recursive: true, force: true });
    mkdirSync(iconset, { recursive: true });
    const sizes = [
      [16, "icon_16x16.png"],
      [32, "icon_16x16@2x.png"],
      [32, "icon_32x32.png"],
      [64, "icon_32x32@2x.png"],
      [128, "icon_128x128.png"],
      [256, "icon_128x128@2x.png"],
      [256, "icon_256x256.png"],
      [512, "icon_256x256@2x.png"],
      [512, "icon_512x512.png"],
      [1024, "icon_512x512@2x.png"],
    ];
    for (const [px, name] of sizes) {
      sipsResize(iconOut, path.join(iconset, name), px);
    }
    const r = spawnSync("iconutil", ["-c", "icns", iconset, "-o", icnsOut], {
      stdio: "ignore",
    });
    if (r.status !== 0 || !existsSync(icnsOut)) {
      console.warn("iconutil failed; electron-builder will convert icon.png");
    }
  }
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
if (process.platform === "darwin" && !existsSync(icnsOut)) {
  die("failed to build icon.icns");
}
stageMac();
const win = stageWin();
if (requireWin && !win) {
  die(
    "缺少 Windows q38.exe。先装 cargo-xwin（或 mingw-w64）再跑 scripts/build-sidecars.sh",
  );
}
console.log(win ? "staged mac + win sidecars" : "staged mac sidecar (no Windows q38.exe yet)");
