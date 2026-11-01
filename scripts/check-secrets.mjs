import { execFileSync } from 'node:child_process'
import { readFileSync, statSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const files = execFileSync('git', ['ls-files', '-z'], {
  cwd: root,
  encoding: 'utf8',
  maxBuffer: 32 * 1024 * 1024
})
  .split('\0')
  .filter(Boolean)

const patterns = [
  // Real keys are long random base62 blobs and virtually always contain a
  // digit; requiring one keeps readable placeholders like `sk-secret-...` out.
  {
    label: 'OpenAI-style secret',
    pattern: /\bsk-(?:proj-|svcacct-|admin-)?(?=[A-Za-z0-9_-]*\d)[A-Za-z0-9_-]{20,}\b/u
  },
  { label: 'AWS access key', pattern: /\bAKIA[0-9A-Z]{16}\b/u },
  { label: 'private key block', pattern: /-----BEGIN [A-Z ]*PRIVATE KEY-----/u }
]

const findings = []
for (const file of files) {
  const path = join(root, file)
  let stats
  try {
    stats = statSync(path)
  } catch {
    continue
  }
  if (!stats.isFile() || stats.size > 2 * 1024 * 1024) continue
  let content
  try {
    content = readFileSync(path, 'utf8')
  } catch {
    continue
  }
  if (content.includes('\0')) continue
  for (const { label, pattern } of patterns) {
    if (pattern.test(content)) findings.push(`${label}: ${file}`)
  }
}

if (findings.length > 0) {
  console.error('Potential secrets found in tracked files:')
  for (const finding of findings) console.error(`- ${finding}`)
  process.exit(1)
}
console.log(`secret scan passed (${files.length} tracked files)`)
