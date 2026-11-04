import { execFileSync } from 'node:child_process'
import {
  cpSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  rmSync,
  statSync
} from 'node:fs'
import { basename, dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const scriptDirectory = dirname(fileURLToPath(import.meta.url))
const root = resolve(scriptDirectory, '..')
const packageJson = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'))
const args = process.argv.slice(2)
let version = packageJson.version

if (args.length > 0) {
  if (args.length !== 2 || args[0] !== '--version') {
    throw new Error('Usage: node scripts/export-dev-source.mjs [--version <source-package-version>]')
  }
  version = args[1]
}

if (typeof version !== 'string' || !/^(?:\d+\.){2,3}\d+$/u.test(version)) {
  throw new Error(`source package version is invalid: ${String(version)}`)
}

const exportRoot = join(root, 'portable-dev-sources')
const bundleName = `TextLens-${version}-dev-source`
const outputDirectory = join(exportRoot, bundleName)

const includeEntries = [
  '.env.example',
  '.github',
  '.gitignore',
  'LICENSE.selection-hook',
  'README.md',
  'README.en.md',
  'THIRD_PARTY_NOTICES.md',
  'DEVELOPMENT_HANDBOOK.md',
  'REFACTOR_PLAN.md',
  'apps',
  'assets',
  'deny.toml',
  'package.json',
  'pnpm-lock.yaml',
  'pnpm-workspace.yaml',
  'scripts',
  'src',
  'src-tauri',
  'tsconfig.json',
  'tsconfig.node.json',
  'tsconfig.web.json',
  'vite.config.ts',
  'vitest.config.ts',
  'vitest.setup.ts'
]

function normalize(path) {
  return path.replace(/\\/gu, '/')
}

function shouldCopy(sourcePath) {
  const relativePath = normalize(relative(root, sourcePath))
  if (!relativePath || relativePath === '') return true

  const name = basename(sourcePath)
  const lowerName = name.toLowerCase()
  if (
    lstatSync(sourcePath).isSymbolicLink()
    || (lowerName.startsWith('.env') && lowerName !== '.env.example')
    || /\.(?:p12|pfx|pem|key|cer|crt|provisionprofile|mobileprovision|jks|keystore)$/u.test(lowerName)
    || /^(?:id_rsa|id_dsa|id_ecdsa|id_ed25519)$/u.test(lowerName)
    || name === '.DS_Store'
    || name === '.git'
    || name === '.pnpm-store'
    || name === '.tmp-textlens-start-diagnostic'
    || name === 'artifacts'
    || name === 'coverage'
    || name === 'dist'
    || name === 'node_modules'
    || name === 'out'
    || name === 'portable-dev-sources'
    || name === 'release'
    || name === 'target'
    || name === '.vite'
    || /^archive-pre-/u.test(name)
    || name.endsWith('.tsbuildinfo')
    || name.endsWith('.log')
  ) {
    return false
  }

  return true
}

function copyEntry(relativeEntry) {
  const source = join(root, relativeEntry)
  const destination = join(outputDirectory, relativeEntry)
  if (!existsSync(source)) {
    throw new Error(`Required entry is missing: ${relativeEntry}`)
  }
  cpSync(source, destination, {
    recursive: true,
    force: true,
    filter: (sourcePath) => shouldCopy(sourcePath)
  })
}

function requireExportedPath(relativeEntry, kind) {
  const path = join(outputDirectory, relativeEntry)
  if (!existsSync(path)) {
    throw new Error(`Export validation failed: missing ${kind} ${relativeEntry}`)
  }
  if (kind === 'file' && !statSync(path).isFile()) {
    throw new Error(`Export validation failed: ${relativeEntry} is not a file`)
  }
  if (kind === 'directory' && !statSync(path).isDirectory()) {
    throw new Error(`Export validation failed: ${relativeEntry} is not a directory`)
  }
}

rmSync(outputDirectory, { recursive: true, force: true })
mkdirSync(outputDirectory, { recursive: true })

for (const entry of includeEntries) {
  copyEntry(entry)
}

for (const [entry, kind] of [
  ['apps', 'directory'],
  ['assets', 'directory'],
  ['.github', 'directory'],
  ['DEVELOPMENT_HANDBOOK.md', 'file'],
  ['deny.toml', 'file'],
  ['src', 'directory'],
  ['src-tauri', 'directory'],
  ['package.json', 'file'],
  ['pnpm-lock.yaml', 'file'],
  ['README.md', 'file'],
  ['README.en.md', 'file']
]) {
  requireExportedPath(entry, kind)
}

for (const excluded of [
  'artifacts',
  'dist',
  'node_modules',
  'portable-dev-sources',
  'release',
  'src-tauri/target'
]) {
  if (existsSync(join(outputDirectory, excluded))) {
    throw new Error(`Export validation failed: excluded path copied into bundle: ${excluded}`)
  }
}

// Scan the finished source tree itself, including untracked files copied by
// the exporter. Remove a failed output so it is not accidentally shared.
try {
  execFileSync(process.execPath, [join(outputDirectory, 'scripts', 'check-secrets.mjs')], {
    cwd: outputDirectory,
    stdio: 'inherit'
  })
} catch (error) {
  rmSync(outputDirectory, { recursive: true, force: true })
  throw new Error('Export validation failed: secret scan did not pass', { cause: error })
}

console.log(`Portable development source created at: ${outputDirectory}`)
