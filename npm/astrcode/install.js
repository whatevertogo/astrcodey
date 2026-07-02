#!/usr/bin/env node
// Post-install script: resolve the correct platform-specific binary package.

const {
  existsSync,
  mkdirSync,
  copyFileSync,
  chmodSync,
  linkSync,
  unlinkSync,
} = require('fs');
const { join } = require('path');

const PLATFORM_MAP = {
  'linux-x64': '@whatevertogo/astrcode-linux-x64',
  'linux-arm64': '@whatevertogo/astrcode-linux-arm64',
  'darwin-x64': '@whatevertogo/astrcode-darwin-x64',
  'darwin-arm64': '@whatevertogo/astrcode-darwin-arm64',
  'win32-x64': '@whatevertogo/astrcode-win32-x64',
  'win32-arm64': '@whatevertogo/astrcode-win32-arm64',
};

const platform = process.platform;
const arch = process.arch;
const key = `${platform}-${arch}`;
const pkg = PLATFORM_MAP[key];

if (!pkg) {
  console.error(`Unsupported platform: ${key}`);
  process.exit(1);
}

let binaryPath;
try {
  const pkgDir = require.resolve(`${pkg}/package.json`);
  const pkgRoot = join(pkgDir, '..');
  const ext = platform === 'win32' ? '.exe' : '';
  binaryPath = join(pkgRoot, `astrcode${ext}`);
} catch (e) {
  console.error(`Failed to find platform package ${pkg}. Try reinstalling.`);
  process.exit(1);
}

if (!existsSync(binaryPath)) {
  console.error(`Binary not found at ${binaryPath}`);
  process.exit(1);
}

const binDir = join(__dirname, 'bin');
if (!existsSync(binDir)) {
  mkdirSync(binDir, { recursive: true });
}

const ext = platform === 'win32' ? '.exe' : '';
const dest = join(binDir, `astrcode${ext}`);
try {
  if (existsSync(dest)) {
    unlinkSync(dest);
  }
  linkSync(binaryPath, dest);
} catch (e) {
  copyFileSync(binaryPath, dest);
}
if (platform !== 'win32') {
  chmodSync(dest, 0o755);
}
