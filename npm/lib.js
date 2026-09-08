"use strict";

const fs = require("fs");
const os = require("os");
const path = require("path");

const REPO = process.env.GROK_TOKENS_REPO || "gxgxhdu60/grok-tokens";
const PKG_DIR = path.join(__dirname, "..");
const VENDOR_DIR = path.join(__dirname, "vendor");
const PKG = (() => {
  try {
    return JSON.parse(fs.readFileSync(path.join(PKG_DIR, "package.json"), "utf8"));
  } catch {
    return { version: "0.1.2" };
  }
})();

const TARGETS = {
  "linux-x64": { triple: "x86_64-unknown-linux-musl", fallback: "x86_64-unknown-linux-gnu", bin: "grok-tokens" },
  "linux-arm64": { triple: "aarch64-unknown-linux-musl", fallback: "aarch64-unknown-linux-gnu", bin: "grok-tokens" },
  "darwin-arm64": { triple: "aarch64-apple-darwin", bin: "grok-tokens" },
  "darwin-x64": { triple: "x86_64-apple-darwin", bin: "grok-tokens" },
  "win32-x64": { triple: "x86_64-pc-windows-msvc", bin: "grok-tokens.exe" },
  "win32-arm64": { triple: "aarch64-pc-windows-msvc", bin: "grok-tokens.exe" },
};

function detectTarget() {
  const plat = process.platform;
  let arch = process.arch;
  if (arch === "ia32") arch = "x64";
  return TARGETS[`${plat}-${arch}`] || null;
}

function vendorBinaryPath() {
  const t = detectTarget();
  if (!t) return null;
  return path.join(VENDOR_DIR, t.bin);
}

function localReleaseBinary() {
  const t = detectTarget();
  const name = t ? t.bin : process.platform === "win32" ? "grok-tokens.exe" : "grok-tokens";
  const p = path.join(PKG_DIR, "target", "release", name);
  return fs.existsSync(p) ? p : null;
}

function hasCargoToml() {
  return fs.existsSync(path.join(PKG_DIR, "Cargo.toml"));
}

function githubHeaders() {
  const headers = {
    "User-Agent": "grok-tokens-npm",
    Accept: "application/vnd.github+json",
  };
  const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
  if (token) headers.Authorization = `Bearer ${token}`;
  return headers;
}

async function latestTag() {
  const res = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
    headers: githubHeaders(),
  });
  if (!res.ok) return null;
  const json = await res.json();
  return json && json.tag_name ? String(json.tag_name) : null;
}

function triplesFor(target) {
  const out = [target.triple];
  if (target.fallback) out.push(target.fallback);
  return out;
}

function tmpDir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "grok-tokens-"));
}

module.exports = {
  REPO,
  PKG,
  PKG_DIR,
  VENDOR_DIR,
  detectTarget,
  vendorBinaryPath,
  localReleaseBinary,
  hasCargoToml,
  githubHeaders,
  latestTag,
  triplesFor,
  tmpDir,
};

if (require.main === module) {
  const t = detectTarget();
  if (!t) {
    console.error(`Unsupported platform: ${process.platform} ${process.arch}`);
    process.exit(1);
  }
  console.log(`${t.triple} (${t.bin})`);
}
