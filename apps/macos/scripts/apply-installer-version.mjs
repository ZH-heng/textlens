#!/usr/bin/env node
// 将构建产物改写为多段安装版本号（如 0.4.2.1）。
//
// Tauri 配置只接受严格的三段 semver，因此四段安装版本号不写入
// tauri.conf.json / package.json，而是在产物打包完成后由本脚本应用：
// 改写 Info.plist 版本键、临时重新 ad-hoc 签名，并用 Tauri 自带的
// bundle_dmg.sh 以相同布局重建 DMG。
//
// 用法：TEXTLENS_INSTALLER_VERSION=0.4.2.1 node apps/macos/scripts/apply-installer-version.mjs
// 校验：pnpm verify:artifacts 会读取同名环境变量核对产物版本。

import { execFileSync } from 'node:child_process'
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, symlinkSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../../..')
const tauriConfig = JSON.parse(readFileSync(join(root, 'src-tauri/tauri.conf.json'), 'utf8'))
const productName = tauriConfig.productName ?? 'TextLens'
const baseVersion = tauriConfig.version
const installerVersion = process.env.TEXTLENS_INSTALLER_VERSION?.trim()

if (!installerVersion) {
  console.error('缺少 TEXTLENS_INSTALLER_VERSION 环境变量，例如：TEXTLENS_INSTALLER_VERSION=0.4.2.1')
  process.exit(1)
}
if (!/^\d+(\.\d+){1,3}$/.test(installerVersion)) {
  console.error(`安装版本号格式不合法：${installerVersion}（期望 1-4 段数字，如 0.4.2.1）`)
  process.exit(1)
}

const targetTriple = 'aarch64-apple-darwin'
const bundleRoot = join(root, 'src-tauri', 'target', targetTriple, 'release', 'bundle')
const appPath = join(bundleRoot, 'macos', `${productName}.app`)
const dmgDirectory = join(bundleRoot, 'dmg')
const dmgScript = join(dmgDirectory, 'bundle_dmg.sh')
const volumeIcon = join(dmgDirectory, 'icon.icns')
const expectedDmg = join(dmgDirectory, `${productName}_${installerVersion}_aarch64.dmg`)
const staleDmg = join(dmgDirectory, `${productName}_${baseVersion}_aarch64.dmg`)

if (!existsSync(appPath)) {
  console.error(`未找到已构建的应用，请先运行 pnpm build：${relative(root, appPath)}`)
  process.exit(1)
}

function run(command, args) {
  execFileSync(command, args, { stdio: 'inherit' })
}

const infoPlist = join(appPath, 'Contents', 'Info.plist')
for (const key of ['CFBundleShortVersionString', 'CFBundleVersion']) {
  run('/usr/bin/plutil', ['-replace', key, '-string', installerVersion, infoPlist])
}

// 改写 plist 会破坏原有密封，按本地分发的 ad-hoc 方式重新签名。
run('/usr/bin/codesign', ['--force', '--sign', '-', appPath])

if (existsSync(expectedDmg)) rmSync(expectedDmg)
const staging = join(dmgDirectory, `staging-${installerVersion}`)
rmSync(staging, { recursive: true, force: true })
mkdirSync(staging, { recursive: true })
cpSync(appPath, join(staging, `${productName}.app`), { recursive: true })

if (existsSync(dmgScript)) {
  // bundle_dmg.sh 的 --app-drop-link 会在卷内自行创建 Applications 链接，
  // staging 里不要再预先放置，否则 ln 会重复报错。
  const args = [
    '--volname', productName,
    '--icon', `${productName}.app`, '180', '170',
    '--app-drop-link', '480', '170',
    '--window-size', '660', '400',
    '--hide-extension', `${productName}.app`
  ]
  if (existsSync(volumeIcon)) args.push('--volicon', volumeIcon)
  run('/bin/bash', [dmgScript, ...args, expectedDmg, staging])
} else {
  symlinkSync('/Applications', join(staging, 'Applications'))
  run('/usr/bin/hdiutil', [
    'create', '-volname', productName, '-srcfolder', staging, '-ov', '-format', 'UDZO', expectedDmg
  ])
}
rmSync(staging, { recursive: true, force: true })

if (installerVersion !== baseVersion && existsSync(staleDmg)) {
  rmSync(staleDmg)
}

console.log(`安装包版本已应用：${relative(root, expectedDmg)}`)

// 用同一环境变量核对改写后的产物。
run('node', [join(root, 'apps/macos/scripts/verify-artifacts.mjs')])
