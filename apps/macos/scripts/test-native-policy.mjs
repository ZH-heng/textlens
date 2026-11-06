import { execFileSync } from 'node:child_process'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

if (process.platform !== 'darwin') {
  console.log('native selection policy tests skipped (macOS only)')
  process.exit(0)
}

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../../..')
const directory = mkdtempSync(join(tmpdir(), 'textlens-selection-policy-'))
try {
  const executable = join(directory, 'selection-policy-test')
  execFileSync(
    'c++',
    [
      '-std=c++17',
      '-Wall',
      '-Wextra',
      '-Werror',
      '-I',
      join(root, 'apps/macos/native'),
      join(root, 'apps/macos/native/tests/selection_policy_test.cc'),
      '-o',
      executable
    ],
    { stdio: 'inherit' }
  )
  execFileSync(executable, [], { stdio: 'inherit' })
  const bridgeExecutable = join(directory, 'selection-bridge-test')
  execFileSync(
    'c++',
    [
      '-std=c++17',
      '-fobjc-arc',
      '-Wall',
      '-Wextra',
      '-Werror',
      '-I',
      join(root, 'apps/macos/native'),
      join(root, 'apps/macos/native/tests/selection_bridge_test.mm'),
      ...['AppKit', 'ApplicationServices', 'Carbon', 'CoreGraphics', 'Foundation'].flatMap(
        (framework) => ['-framework', framework]
      ),
      '-o',
      bridgeExecutable
    ],
    { stdio: 'inherit' }
  )
  execFileSync(bridgeExecutable, [], { stdio: 'inherit' })
  const manualPolicyExecutable = join(directory, 'manual-capture-policy-test')
  execFileSync(
    'rustc',
    [
      '--edition',
      '2021',
      '--test',
      join(root, 'apps/windows/src/manual_capture_policy.rs'),
      '-o',
      manualPolicyExecutable
    ],
    { stdio: 'inherit' }
  )
  execFileSync(manualPolicyExecutable, [], { stdio: 'inherit' })
} finally {
  rmSync(directory, { recursive: true, force: true })
}
