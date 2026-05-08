import { copyFileSync, mkdirSync, statSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url))
const REPO_ROOT = path.resolve(SCRIPT_DIR, '../..')
const SIDECAR_BIN = 'astrcode-http-server'

function parseArgs(argv) {
  return {
    release: argv.includes('--release'),
  }
}

function executableName(name) {
  return process.platform === 'win32' ? `${name}.exe` : name
}

function hostTargetTriple() {
  const platform = os.platform()
  const arch = os.arch()

  if (platform === 'win32' && arch === 'x64') return 'x86_64-pc-windows-msvc'
  if (platform === 'win32' && arch === 'arm64') return 'aarch64-pc-windows-msvc'
  if (platform === 'darwin' && arch === 'x64') return 'x86_64-apple-darwin'
  if (platform === 'darwin' && arch === 'arm64') return 'aarch64-apple-darwin'
  if (platform === 'linux' && arch === 'x64') return 'x86_64-unknown-linux-gnu'
  if (platform === 'linux' && arch === 'arm64')
    return 'aarch64-unknown-linux-gnu'

  throw new Error(`Unsupported sidecar host target: ${platform}/${arch}`)
}

function cargoTargetTriple() {
  return (
    process.env.CARGO_BUILD_TARGET || process.env.TARGET || hostTargetTriple()
  )
}

function cargoProfile(release) {
  return release ? 'release' : 'debug'
}

function cargoTargetDir(targetTriple, profile) {
  if (process.env.CARGO_BUILD_TARGET || process.env.TARGET) {
    return path.join(REPO_ROOT, 'target', targetTriple, profile)
  }
  return path.join(REPO_ROOT, 'target', profile)
}

function runCargoBuild({ release, targetTriple }) {
  const args = ['build', '-p', 'astrcode-server', '--bin', SIDECAR_BIN]
  if (release) args.push('--release')
  if (process.env.CARGO_BUILD_TARGET || process.env.TARGET) {
    args.push('--target', targetTriple)
  }

  const result = spawnSync('cargo', args, {
    cwd: REPO_ROOT,
    stdio: 'inherit',
  })

  if (result.error) {
    throw result.error
  }

  if (result.status !== 0) {
    throw new Error(`cargo ${args.join(' ')} failed`)
  }
}

function copySidecar({ targetTriple, profile }) {
  const exe = executableName(SIDECAR_BIN)
  const source = path.join(cargoTargetDir(targetTriple, profile), exe)
  statSync(source)

  const destinationDir = path.join(REPO_ROOT, 'src-tauri', 'binaries')
  const destination = path.join(
    destinationDir,
    executableName(`${SIDECAR_BIN}-${targetTriple}`)
  )

  mkdirSync(destinationDir, { recursive: true })
  copyFileSync(source, destination)
  console.log(
    `[sidecar] copied ${path.relative(REPO_ROOT, source)} -> ${path.relative(REPO_ROOT, destination)}`
  )
}

const options = parseArgs(process.argv.slice(2))
const targetTriple = cargoTargetTriple()
const profile = cargoProfile(options.release)

runCargoBuild({ release: options.release, targetTriple })
copySidecar({ targetTriple, profile })
