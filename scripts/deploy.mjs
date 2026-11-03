#!/usr/bin/env node
/**
 * dalzzack CLI Installer
 *
 * Builds the project and installs the binary to a PATH-accessible location.
 *
 * Usage:
 *   node scripts/deploy.mjs              # Build + install
 *   node scripts/deploy.mjs --skip-build # Install only (use existing binary)
 *   node scripts/deploy.mjs --target /usr/local/bin  # Custom install path
 */

import { execSync } from "child_process";
import { copyFileSync, existsSync, mkdirSync, chmodSync, readFileSync } from "fs";
import { platform, homedir } from "os";
import { resolve, dirname, join } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const PROJECT_ROOT = join(__dirname, "..");

// --- Parse args ---
const args = process.argv.slice(2);
const skipBuild = args.includes("--skip-build");
const targetIdx = args.indexOf("--target");
const customTarget = targetIdx !== -1 ? args[targetIdx + 1] : null;

// --- Platform detection ---
const os = platform();
const isWindows = os === "win32";
const isMac = os === "darwin";
const ext = isWindows ? ".exe" : "";
const binaryName = "dalzzack" + ext;

// --- Read version from tauri.conf.json ---
function getVersion() {
  try {
    const conf = JSON.parse(readFileSync(join(PROJECT_ROOT, "src-tauri/tauri.conf.json"), "utf-8"));
    return conf.version || "unknown";
  } catch {
    return "unknown";
  }
}

// --- Determine install directory ---
function getInstallDir() {
  if (customTarget) return resolve(customTarget);

  if (isWindows) {
    // Windows: %USERPROFILE%\.cargo\bin or %LOCALAPPDATA%\Programs\dalzzack
    const cargoDir = join(homedir(), ".cargo", "bin");
    if (existsSync(cargoDir)) return cargoDir;
    const localPrograms = join(process.env.LOCALAPPDATA || join(homedir(), "AppData", "Local"), "Programs", "dalzzack");
    return localPrograms;
  }

  // macOS/Linux: ~/.cargo/bin (typically in PATH) > /usr/local/bin
  const cargoDir = join(homedir(), ".cargo", "bin");
  if (existsSync(cargoDir)) return cargoDir;
  return "/usr/local/bin";
}

// --- Find built binary ---
// IMPORTANT: use only the project's own target dir. A path into another checkout
// would silently install a stale binary if present.
function findBinary() {
  const candidates = [
    resolve(PROJECT_ROOT, "src-tauri/target/release", binaryName),
    resolve(PROJECT_ROOT, "src-tauri/target/debug", binaryName),
  ];
  for (const p of candidates) {
    if (existsSync(p)) return p;
  }
  return null;
}

// --- Main ---
const version = getVersion();
console.log(`[deploy] dalzzack v${version}`);
console.log(`[deploy] Platform: ${os}`);
console.log();

// Step 1: Build
if (!skipBuild) {
  // Build frontend first (Tauri needs it)
  console.log("[deploy] Building frontend...");
  try {
    execSync("npm run build", { cwd: PROJECT_ROOT, stdio: "inherit" });
  } catch {
    console.error("[deploy] Frontend build failed.");
    process.exit(1);
  }

  // Build Rust binary only (skip DMG/MSI bundling which often fails)
  console.log("[deploy] Building binary (release)...");
  try {
    execSync("cargo build --release", { cwd: join(PROJECT_ROOT, "src-tauri"), stdio: "inherit" });
  } catch {
    console.error("[deploy] Cargo build failed.");
    process.exit(1);
  }
  console.log();
}

// Step 2: Find binary
const src = findBinary();
if (!src) {
  console.error("[deploy] Binary not found. Run without --skip-build to build first.");
  process.exit(1);
}

// Step 3: Install
const installDir = getInstallDir();
const dest = join(installDir, binaryName);

console.log(`[deploy] Source:  ${src}`);
console.log(`[deploy] Install: ${dest}`);

if (!existsSync(installDir)) {
  mkdirSync(installDir, { recursive: true });
  console.log(`[deploy] Created ${installDir}`);
}

copyFileSync(src, dest);

if (!isWindows) {
  chmodSync(dest, 0o755);
}

// Step 4: macOS codesign (ad-hoc)
if (isMac) {
  try {
    execSync(`codesign --force --sign - "${dest}"`, { stdio: "pipe" });
    console.log("[deploy] Codesigned (ad-hoc)");
  } catch {
    console.log("[deploy] Codesign skipped (non-critical)");
  }
}

// Step 5: Verify PATH
const inPath = (process.env.PATH || "").split(isWindows ? ";" : ":").some(
  (p) => resolve(p) === resolve(installDir)
);

console.log();
console.log(`[deploy] Installed dalzzack v${version} to ${dest}`);

if (inPath) {
  console.log("[deploy] Ready to use: dalzzack --help");
} else {
  console.log();
  console.log(`[deploy] WARNING: ${installDir} is not in your PATH.`);
  if (isWindows) {
    console.log(`  Add to PATH: setx PATH "%PATH%;${installDir}"`);
  } else {
    console.log(`  Add to PATH: export PATH="${installDir}:$PATH"`);
    console.log(`  (Add the above to ~/.zshrc or ~/.bashrc to persist)`);
  }
}
