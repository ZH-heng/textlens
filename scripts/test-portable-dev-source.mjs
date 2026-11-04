import assert from 'node:assert/strict'
import { execFileSync, spawnSync } from 'node:child_process'
import { cpSync, existsSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { test } from 'node:test'

const scripts = dirname(fileURLToPath(import.meta.url))
const fakeSecret = 'sk-' + 'a'.repeat(20) + '1'

function runScript(root, script) {
  return spawnSync(process.execPath, [join(root, 'scripts', script)], {
    cwd: root,
    encoding: 'utf8'
  })
}

function makeFile(root, path, content = 'fixture\n') {
  const destination = join(root, path)
  mkdirSync(dirname(destination), { recursive: true })
  writeFileSync(destination, content)
}

function makeFixture(root) {
  mkdirSync(join(root, 'scripts'), { recursive: true })
  for (const script of ['check-secrets.mjs', 'export-dev-source.mjs', 'test-portable-dev-source.mjs']) {
    cpSync(join(scripts, script), join(root, 'scripts', script))
  }
  for (const file of [
    '.env.example', '.gitignore', '.github/workflows/verify.yml',
    'LICENSE.selection-hook', 'README.md', 'README.en.md', 'THIRD_PARTY_NOTICES.md',
    'DEVELOPMENT_HANDBOOK.md', 'REFACTOR_PLAN.md', 'deny.toml', 'pnpm-lock.yaml',
    'pnpm-workspace.yaml', 'tsconfig.json', 'tsconfig.node.json', 'tsconfig.web.json',
    'vite.config.ts', 'vitest.config.ts', 'vitest.setup.ts', 'apps/app.txt',
    'apps/windows/SELECTION_VALIDATION.md',
    'assets/icon.txt', 'src/app.txt', 'src-tauri/app.txt'
  ]) makeFile(root, file)
  makeFile(root, 'package.json', '{"version":"0.4.2"}\n')
}

test('archive scanner checks its own files without Git and ignores build caches', (t) => {
  const root = mkdtempSync(join(tmpdir(), 'textlens-source-scan-'))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  makeFixture(root)
  makeFile(root, 'node_modules/package/data.txt', fakeSecret)
  let result = runScript(root, 'check-secrets.mjs')
  assert.equal(result.status, 0, result.stderr)
  assert.match(result.stdout, /source files/u)
  makeFile(root, 'src/untracked.txt', fakeSecret)
  result = runScript(root, 'check-secrets.mjs')
  assert.equal(result.status, 1)
  assert.match(result.stderr, /src\/untracked\.txt/u)
})

test('archive inside another Git repository scans the archive, not its parent', (t) => {
  const outer = mkdtempSync(join(tmpdir(), 'textlens-parent-git-'))
  t.after(() => rmSync(outer, { recursive: true, force: true }))
  execFileSync('git', ['init', '--quiet', outer])
  makeFile(outer, 'outside.txt', 'outer repository\n')
  execFileSync('git', ['add', 'outside.txt'], { cwd: outer })
  const root = join(outer, 'archive')
  makeFixture(root)
  makeFile(root, 'src/untracked.txt', fakeSecret)
  const result = runScript(root, 'check-secrets.mjs')
  assert.equal(result.status, 1)
  assert.match(result.stderr, /src\/untracked\.txt/u)
})

test('a Git project retains tracked-file scanning semantics', (t) => {
  const root = mkdtempSync(join(tmpdir(), 'textlens-project-git-'))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  makeFixture(root)
  execFileSync('git', ['init', '--quiet', root])
  execFileSync('git', ['add', '.'], { cwd: root })
  makeFile(root, 'src/untracked.txt', fakeSecret)
  let result = runScript(root, 'check-secrets.mjs')
  assert.equal(result.status, 0, result.stderr)
  assert.match(result.stdout, /tracked files/u)
  makeFile(root, 'src/app.txt', fakeSecret)
  result = runScript(root, 'check-secrets.mjs')
  assert.equal(result.status, 1)
  assert.match(result.stderr, /src\/app\.txt/u)
  assert.doesNotMatch(result.stderr, /src\/untracked\.txt/u)
})

test('export retains project configuration, omits secrets and validates exported contents', (t) => {
  const root = mkdtempSync(join(tmpdir(), 'textlens-export-'))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  makeFixture(root)
  makeFile(root, 'apps/nested/.env.local', fakeSecret)
  makeFile(root, 'apps/nested/private.p12', fakeSecret)
  makeFile(root, 'apps/nested/id_rsa', fakeSecret)
  makeFile(root, 'apps/nested/node_modules/package/data.txt', fakeSecret)
  let symlinkCreated = true
  try {
    symlinkSync(join(root, 'apps', 'nested', '.env.local'), join(root, 'apps', 'nested', 'linked.txt'))
  } catch (error) {
    if (error.code !== 'EPERM' && error.code !== 'EACCES') throw error
    symlinkCreated = false
  }

  const output = join(root, 'portable-dev-sources', 'TextLens-0.4.2-dev-source')
  let result = runScript(root, 'export-dev-source.mjs')
  assert.equal(result.status, 0, result.stderr)
  for (const file of [
    '.github/workflows/verify.yml', 'deny.toml', '.env.example',
    'scripts/test-portable-dev-source.mjs', 'apps/windows/SELECTION_VALIDATION.md'
  ]) {
    assert.ok(existsSync(join(output, file)), file)
  }
  for (const file of ['apps/nested/.env.local', 'apps/nested/private.p12', 'apps/nested/id_rsa', 'apps/nested/node_modules']) {
    assert.equal(existsSync(join(output, file)), false, file)
  }
  if (symlinkCreated) assert.equal(existsSync(join(output, 'apps/nested/linked.txt')), false)

  makeFile(root, 'src/untracked.txt', fakeSecret)
  result = runScript(root, 'export-dev-source.mjs')
  assert.notEqual(result.status, 0)
  assert.match(result.stderr, /src\/untracked\.txt/u)
  assert.equal(existsSync(output), false)
})
