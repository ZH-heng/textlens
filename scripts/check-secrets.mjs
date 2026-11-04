import { execFileSync } from 'node:child_process'
import { lstatSync, readFileSync, readdirSync, realpathSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
// An exported source tree has no .git. It can also live inside an unrelated
// repository, in which case git ls-files would silently inspect that repository
// instead. Use Git only when its root is this exact project directory.
function trackedFilesAtRoot() {
  try {
    const gitRoot = execFileSync('git', ['rev-parse', '--show-toplevel'], {
      cwd: root,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore']
    }).trim()
    if (realpathSync(gitRoot) !== realpathSync(root)) return null
    return execFileSync('git', ['ls-files', '-z'], {
      cwd: root,
      encoding: 'utf8',
      maxBuffer: 32 * 1024 * 1024
    }).split('\0').filter(Boolean)
  } catch {
    return null
  }
}

// Scan source files in an extracted archive without descending into build
// outputs or following symlinks outside that archive. Unlike Git mode this also
// checks newly created files, since an archive has no tracked-file index.
const generatedDirectories = new Set([
  '.git', '.pnpm-store', '.vite', 'artifacts', 'coverage', 'dist',
  'node_modules', 'out', 'portable-dev-sources', 'release', 'target'
])

function sourceFiles(directory = root, prefix = '') {
  const files = []
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const relativePath = prefix ? `${prefix}/${entry.name}` : entry.name
    if (entry.isDirectory()) {
      if (!generatedDirectories.has(entry.name)) {
        files.push(...sourceFiles(join(directory, entry.name), relativePath))
      }
    } else if (entry.isFile()) {
      files.push(relativePath)
    }
  }
  return files
}

const trackedFiles = trackedFilesAtRoot()
const files = trackedFiles ?? sourceFiles()
const scanDescription = trackedFiles === null ? 'source files' : 'tracked files'

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
    stats = lstatSync(path)
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
  console.error(`Potential secrets found in ${scanDescription}:`)
  for (const finding of findings) console.error(`- ${finding}`)
  process.exit(1)
}
console.log(`secret scan passed (${files.length} ${scanDescription})`)
