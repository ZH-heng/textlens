import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync, statSync } from 'node:fs'
import { dirname, isAbsolute, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../../..')
const packageJson = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'))
const tauriConfig = JSON.parse(readFileSync(join(root, 'src-tauri/tauri.conf.json'), 'utf8'))
const productName = tauriConfig.productName ?? packageJson.productName ?? 'TextLens'
// TEXTLENS_INSTALLER_VERSION 由 apply-installer-version.mjs 的工作流设置，
// 用于核对多段安装版本号（如 0.4.2.1）；package.json 始终核对 Tauri 基础版本。
const baseVersion = tauriConfig.version ?? packageJson.version
const expectedVersion = process.env.TEXTLENS_INSTALLER_VERSION?.trim() || baseVersion
const expectedIdentifier = tauriConfig.identifier
const expectedMinimumVersion = tauriConfig.bundle?.macOS?.minimumSystemVersion ?? '12.0'
const expectedMenuBarOnly = true
const targetTriple = 'aarch64-apple-darwin'
const cargoTargetRoot = process.env.CARGO_TARGET_DIR
  ? isAbsolute(process.env.CARGO_TARGET_DIR)
    ? process.env.CARGO_TARGET_DIR
    : resolve(root, 'src-tauri', process.env.CARGO_TARGET_DIR)
  : join(root, 'src-tauri', 'target')
const bundleRoot = join(cargoTargetRoot, targetTriple, 'release', 'bundle')
const appPath = join(bundleRoot, 'macos', `${productName}.app`)
const infoPlist = join(appPath, 'Contents', 'Info.plist')
const errors = []

if (tauriConfig.build?.devUrl || tauriConfig.build?.beforeDevCommand) {
  errors.push('生产 Tauri 配置不得包含 devUrl 或 beforeDevCommand')
}
if (tauriConfig.app?.security?.devCsp) {
  errors.push('生产 Tauri 配置不得包含 devCsp')
}

if (packageJson.productName !== productName) {
  errors.push(`package.json 与 Tauri 产品名不一致：${packageJson.productName} / ${productName}`)
}
if (packageJson.version !== baseVersion) {
  errors.push(`package.json 与 Tauri 版本不一致：${packageJson.version} / ${baseVersion}`)
}

function requireFile(path, label) {
  if (!existsSync(path) || !statSync(path).isFile()) {
    errors.push(`${label} 不存在：${relative(root, path)}`)
    return false
  }
  return true
}

function plistValue(key) {
  try {
    return execFileSync(
      '/usr/bin/plutil',
      ['-extract', key, 'raw', '-o', '-', infoPlist],
      { encoding: 'utf8' }
    ).trim()
  } catch {
    errors.push(`无法读取 Info.plist 中的 ${key}`)
    return ''
  }
}

function architectures(path) {
  try {
    return execFileSync('/usr/bin/lipo', ['-archs', path], { encoding: 'utf8' })
      .trim()
      .split(/\s+/u)
      .filter(Boolean)
  } catch {
    errors.push(`无法读取 Mach-O 架构：${relative(root, path)}`)
    return []
  }
}

function versionParts(value) {
  return value.split('.').map((part) => Number.parseInt(part, 10) || 0)
}

function versionAtLeast(actual, minimum) {
  const left = versionParts(actual)
  const right = versionParts(minimum)
  const length = Math.max(left.length, right.length)
  for (let index = 0; index < length; index += 1) {
    const difference = (left[index] ?? 0) - (right[index] ?? 0)
    if (difference !== 0) return difference > 0
  }
  return true
}

if (!existsSync(appPath)) {
  errors.push(`未找到 Tauri macOS 应用：${relative(root, appPath)}`)
} else if (requireFile(infoPlist, '应用 Info.plist')) {
  const executableName = plistValue('CFBundleExecutable')
  const executable = join(appPath, 'Contents', 'MacOS', executableName)
  if (executableName && requireFile(executable, '应用主可执行文件')) {
    const actualArchitectures = architectures(executable)
    if (actualArchitectures.length !== 1 || actualArchitectures[0] !== 'arm64') {
      errors.push(`应用必须仅包含 arm64，实际为：${actualArchitectures.join(', ') || '未知'}`)
    }

    const executableBytes = readFileSync(executable)
    const forbiddenProductionMarkers = [
      '127.0.0.1:1420',
      'computer-use',
      'node_repl',
      '@oai/sky',
      'mcp__',
      'playwright',
      'vitest',
      'healthcheck',
      '/usr/bin/open',
      root,
      process.env.HOME
    ].filter((marker) => typeof marker === 'string' && marker.length > 0)
    for (const marker of forbiddenProductionMarkers) {
      if (executableBytes.includes(Buffer.from(marker))) {
        errors.push(`应用主程序包含开发或测试标记：${marker}`)
      }
    }
  }

  const identifier = plistValue('CFBundleIdentifier')
  if (identifier && identifier !== expectedIdentifier) {
    errors.push(`Bundle ID 不匹配：期望 ${expectedIdentifier}，实际 ${identifier}`)
  }

  const bundleVersion = plistValue('CFBundleShortVersionString')
  if (bundleVersion && bundleVersion !== expectedVersion) {
    errors.push(`应用版本不匹配：期望 ${expectedVersion}，实际 ${bundleVersion}`)
  }

  const minimumVersion = plistValue('LSMinimumSystemVersion')
  if (minimumVersion && !versionAtLeast(minimumVersion, expectedMinimumVersion)) {
    errors.push(`最低系统版本低于 ${expectedMinimumVersion}：实际 ${minimumVersion}`)
  }

  const menuBarOnly = plistValue('LSUIElement')
  if (expectedMenuBarOnly && menuBarOnly !== '1' && menuBarOnly.toLowerCase() !== 'true') {
    errors.push('macOS 应用必须设置 LSUIElement=true，默认不显示在 Dock')
  }

  requireFile(
    join(appPath, 'Contents', 'Resources', 'THIRD_PARTY_NOTICES.md'),
    '第三方许可声明'
  )
  requireFile(
    join(appPath, 'Contents', 'Resources', 'LICENSE.selection-hook'),
    'selection-hook MIT 许可证'
  )
}

const dmgDirectory = join(bundleRoot, 'dmg')
const expectedDmg = join(dmgDirectory, `${productName}_${expectedVersion}_aarch64.dmg`)
if (requireFile(expectedDmg, '当前版本 arm64 DMG') && statSync(expectedDmg).size === 0) {
  errors.push('当前版本 DMG 文件为空')
}

if (errors.length > 0) {
  for (const error of errors) console.error(`- ${error}`)
  process.exitCode = 1
} else {
  console.log(
    `产物检查通过：${productName}.app 为纯 arm64，macOS ${expectedMinimumVersion}+，并已生成 DMG。`
  )
}
