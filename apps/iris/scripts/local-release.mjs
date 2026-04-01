#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import {
  copyFileSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import os from 'node:os'
import { basename, dirname, join, resolve } from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

import {
  assetPrefixForTag,
  autoDetectWindowsVmName,
  buildReleaseManifest,
  normalizeTag,
  parseEnvFile,
  readCargoPackageVersionTag,
  renderReleaseNotes,
  splitCsv,
} from './local-release-lib.mjs'

const __dirname = dirname(fileURLToPath(import.meta.url))
const appDir = resolve(__dirname, '..')
const repoRoot = resolve(appDir, '..', '..')
const appCargoToml = join(appDir, 'src-tauri', 'Cargo.toml')
const distDir = join(repoRoot, 'dist', 'iris-native')
const frontendDistDir = join(appDir, 'dist')
const packagingConfig = 'src-tauri/tauri.release.no-frontend.json'
const dockerfile = join(appDir, 'scripts', 'Dockerfile.native-linux-release')
const defaultEnvFiles = [
  join(repoRoot, '.env.release.local'),
  join(appDir, '.env.release.local'),
]
const installArgs = ['install', '--frozen-lockfile', '--ignore-scripts']
const macosSigningRequiredEnv = [
  'MACOS_SIGNING_IDENTITY',
  'MACOS_CERTIFICATE_P12',
  'MACOS_CERTIFICATE_PASSWORD',
]
const macosNotarizationRequiredEnv = [
  'MACOS_NOTARIZE_APPLE_ID',
  'MACOS_NOTARIZE_APP_PASSWORD',
  'MACOS_NOTARIZE_TEAM_ID',
]

class SkipStepError extends Error {}

export function usage() {
  return `Usage: node apps/iris/scripts/local-release.mjs [options]

Build locally-available Iris desktop release artifacts, stage a hashtree release directory,
and optionally publish it.

Options:
  --publish                 Publish the staged release tree with htree
  --dry-run                 Print the plan without running build or publish commands
  --skip-verify            Skip frontend verification
  --tag <tag>              Release tag (defaults to apps/iris/src-tauri/Cargo.toml version, for example v0.2.14)
  --release-tree <name>    Mutable release tree name to publish into
  --stage-dir <path>       Directory used for staged release metadata
  --env-file <path>        Extra dotenv file to load (repeatable)
  --only <csv>             Limit steps to verify,macos,linux,windows
  --skip <csv>             Skip steps by name
  --allow-unsigned-macos   Build the macOS app without signing when signing inputs are unavailable
  --help                   Show this help

Notes:
  - macOS app bundles build locally on Apple Silicon macOS.
  - Signed macOS releases use MACOS_* signing/notarization env vars from the shell or .env.release.local.
  - Linux bundles build natively on Linux or inside Docker elsewhere.
  - Windows installers build inside a Parallels Windows VM when available.
  - --publish requires an explicit --release-tree so partial app-only releases do not
    accidentally overwrite the repo's combined release tree.`
}

export function parseArgs(argv) {
  const args = [...argv].filter((arg, index) => !(arg === '--' && index === 0))
  const options = {
    dryRun: false,
    publish: false,
    skipVerify: false,
    releaseTree: null,
    stageDir: null,
    tag: null,
    envFiles: [],
    only: null,
    skip: new Set(),
    allowUnsignedMacos: false,
  }

  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index]
    switch (arg) {
      case '--help':
      case '-h':
        return { help: true }
      case '--publish':
        options.publish = true
        break
      case '--dry-run':
        options.dryRun = true
        break
      case '--skip-verify':
        options.skipVerify = true
        break
      case '--tag':
        options.tag = normalizeTag(args[++index] ?? '')
        break
      case '--release-tree':
        options.releaseTree = args[++index] ?? ''
        break
      case '--stage-dir':
        options.stageDir = args[++index] ?? ''
        break
      case '--env-file':
        options.envFiles.push(resolve(repoRoot, args[++index] ?? ''))
        break
      case '--only':
        options.only = new Set(splitCsv(args[++index] ?? ''))
        break
      case '--skip':
        for (const value of splitCsv(args[++index] ?? '')) {
          options.skip.add(value)
        }
        break
      case '--allow-unsigned-macos':
        options.allowUnsignedMacos = true
        break
      default:
        throw new Error(`Unknown argument: ${arg}`)
    }
  }

  return options
}

export function windowsArtifactArch(targetTriple) {
  if (targetTriple.startsWith('x86_64-')) {
    return 'x64'
  }
  if (targetTriple.startsWith('aarch64-')) {
    return 'arm64'
  }

  return targetTriple
}

export function workspaceInstallCommands(pnpmCommand = 'pnpm') {
  return [`${pnpmCommand} ${installArgs.join(' ')}`]
}

export function windowsTauriBuildCommand(
  target,
  {
    relativeAppDir = 'apps\\iris',
    configPath = packagingConfigPath(),
  } = {},
) {
  return [
    `& ${psQuote(`.\\${relativeAppDir}\\node_modules\\.bin\\tauri.cmd`)}`,
    'build',
    '--config',
    psQuote(configPath),
    '--target',
    psQuote(target),
    '--bundles',
    'nsis',
    '--ci',
  ].join(' ')
}

export function linuxDockerShellCommand(target = 'x86_64-unknown-linux-gnu') {
  return [
    'set -euo pipefail',
    'export CI=true',
    'pnpm config set store-dir /pnpm/store',
    ...workspaceInstallCommands('pnpm'),
    `pnpm --dir apps/iris exec tauri build --config ${quote(packagingConfigPath())} --target ${target} --bundles appimage,deb --ci`,
  ].join(' && ')
}

export function linuxDockerVolumeMounts(currentRepoRoot = repoRoot) {
  return [
    `${currentRepoRoot}:/workspace`,
    'iris-browser-iris-release-node-modules:/workspace/node_modules',
    'iris-browser-iris-release-pnpm-store:/pnpm/store',
    'iris-browser-iris-release-target:/workspace/apps/iris/src-tauri/target',
    'iris-browser-iris-release-cargo-registry:/root/.cargo/registry',
    'iris-browser-iris-release-cargo-git:/root/.cargo/git',
  ]
}

export function packagingConfigPath() {
  return packagingConfig
}

export function defaultSharedWindowsRepoPath(currentRepoRoot = repoRoot) {
  if (process.platform !== 'darwin') {
    return null
  }

  const homeDir = os.homedir()
  if (!currentRepoRoot.startsWith(`${homeDir}/`)) {
    return null
  }

  const relative = currentRepoRoot.slice(homeDir.length + 1).split('/').join('\\')
  return `C:\\Mac\\Home\\${relative}`
}

function readOptionalEnvFiles(envFiles) {
  const loaded = {}
  const loadedPaths = []

  for (const envFile of envFiles) {
    if (!existsSync(envFile)) {
      continue
    }

    Object.assign(loaded, parseEnvFile(readFileSync(envFile, 'utf8')))
    loadedPaths.push(envFile)
  }

  return { loaded, loadedPaths }
}

function commandExists(command) {
  const result =
    process.platform === 'win32'
      ? spawnSync('where', [command], { stdio: 'ignore' })
      : spawnSync('sh', ['-lc', `command -v "${command}"`], { stdio: 'ignore' })

  return result.status === 0
}

function quote(arg) {
  const value = String(arg)
  return /[^\w./:-]/.test(value) ? JSON.stringify(value) : value
}

function envFlagEnabled(value) {
  return /^(1|true|yes|on)$/i.test(String(value ?? '').trim())
}

function missingEnvVars(names, env) {
  return names.filter((name) => !String(env[name] ?? '').trim())
}

function detectLocalMacosReleaseCapabilities(env) {
  const missingSigning = missingEnvVars(macosSigningRequiredEnv, env)
  const missingNotarization = missingEnvVars(macosNotarizationRequiredEnv, env)

  return {
    signingReady: missingSigning.length === 0,
    notarizationReady: missingNotarization.length === 0,
    missingSigning,
    missingNotarization,
  }
}

function run(command, args, { cwd = repoRoot, env = process.env, capture = false, dryRun = false } = {}) {
  const rendered = [command, ...args].map(quote).join(' ')
  console.log(`$ ${rendered}`)
  if (dryRun) {
    return ''
  }

  const result = spawnSync(command, args, {
    cwd,
    env,
    encoding: 'utf8',
    stdio: capture ? 'pipe' : 'inherit',
  })

  if (result.status !== 0) {
    const stderr = capture ? result.stderr.trim() : ''
    throw new Error(stderr || `${command} exited with status ${result.status ?? 'unknown'}`)
  }

  return capture ? result.stdout.trim() : ''
}

function resolveHostPnpmInvocation() {
  if (commandExists('pnpm')) {
    return ['pnpm']
  }
  if (commandExists('corepack')) {
    return ['corepack', 'pnpm']
  }

  throw new Error('Missing pnpm (or corepack) on the local host')
}

function runPnpm(pnpmInvocation, args, options = {}) {
  const [command, ...prefix] = pnpmInvocation
  return run(command, [...prefix, ...args], options)
}

function installFrontendDependencies(pnpmInvocation, { dryRun }) {
  runPnpm(pnpmInvocation, installArgs, { cwd: repoRoot, dryRun })
}

function ensureFrontendDistAvailable(dryRun) {
  if (dryRun) {
    return
  }
  if (!existsSync(join(frontendDistDir, 'index.html'))) {
    throw new Error('Missing apps/iris/dist. Run verify first or build the frontend before packaging.')
  }
}

function ensureDistDir(dryRun) {
  if (!dryRun) {
    mkdirSync(distDir, { recursive: true })
  }
}

function findFirstFile(root, matcher) {
  if (!existsSync(root)) {
    return null
  }

  const entries = readdirSync(root).sort()
  const match = entries.find((entry) => matcher(entry))
  return match ? join(root, match) : null
}

function findBundleArtifact(candidates, subdir, matcher) {
  for (const candidate of candidates) {
    const file = findFirstFile(join(candidate, subdir), matcher)
    if (file) {
      return file
    }
  }

  return null
}

function appBundleCandidates(target) {
  return [
    join(appDir, 'src-tauri', 'target', target, 'release', 'bundle'),
    join(appDir, 'src-tauri', 'target', 'release', 'bundle'),
    join(repoRoot, 'target', target, 'release', 'bundle'),
    join(repoRoot, 'target', 'release', 'bundle'),
  ]
}

function psQuote(value) {
  return `'${String(value).replace(/'/g, "''")}'`
}

function prepareLocalMacosSigning(env, { dryRun }) {
  const tempRoot = dryRun
    ? join(os.tmpdir(), 'iris-local-signing-dry-run')
    : mkdtempSync(join(os.tmpdir(), 'iris-local-signing-'))
  const keychainPath = join(tempRoot, 'iris-signing.keychain-db')
  const certPath = join(tempRoot, 'iris-signing-cert.p12')
  const keychainPassword = env.MACOS_KEYCHAIN_PASSWORD || 'temp_signing_password'

  if (!dryRun) {
    writeFileSync(certPath, Buffer.from(env.MACOS_CERTIFICATE_P12, 'base64'))
  }

  run('security', ['create-keychain', '-p', keychainPassword, keychainPath], { dryRun })
  run('security', ['set-keychain-settings', '-lut', '21600', keychainPath], { dryRun })
  run('security', ['unlock-keychain', '-p', keychainPassword, keychainPath], { dryRun })
  run(
    'security',
    [
      'import',
      certPath,
      '-k',
      keychainPath,
      '-P',
      env.MACOS_CERTIFICATE_PASSWORD,
      '-T',
      '/usr/bin/codesign',
      '-T',
      '/usr/bin/security',
    ],
    { dryRun },
  )
  run(
    'security',
    ['set-key-partition-list', '-S', 'apple-tool:,apple:', '-k', keychainPassword, keychainPath],
    { dryRun },
  )

  const identities = run('security', ['find-identity', '-v', '-p', 'codesigning', keychainPath], {
    capture: true,
    dryRun,
  })
  if (!dryRun && !identities.includes(env.MACOS_SIGNING_IDENTITY)) {
    throw new Error(`Expected signing identity not found: ${env.MACOS_SIGNING_IDENTITY}`)
  }

  return {
    keychainPath,
    cleanup() {
      if (dryRun) {
        return
      }
      rmSync(certPath, { force: true })
      run('security', ['delete-keychain', keychainPath], { dryRun })
      rmSync(tempRoot, { recursive: true, force: true })
    },
  }
}

function signLocalMacosApp({ appPath, env, keychainPath, dryRun }) {
  const entitlementsPath = join(appDir, 'src-tauri', 'Release.entitlements')
  const args = ['--force', '--deep', '--options', 'runtime', '--timestamp', '--keychain', keychainPath]
  if (existsSync(entitlementsPath)) {
    args.push('--entitlements', entitlementsPath)
  }
  args.push('--sign', env.MACOS_SIGNING_IDENTITY, appPath)

  run('codesign', args, { dryRun })
  run('codesign', ['--verify', '--deep', '--strict', '--verbose=2', appPath], { dryRun })
}

function notarizeLocalMacosApp({ appPath, env, dryRun }) {
  const tempRoot = dryRun
    ? join(os.tmpdir(), 'iris-local-notary-dry-run')
    : mkdtempSync(join(os.tmpdir(), 'iris-local-notary-'))
  const notaryZipPath = join(tempRoot, 'iris-notarize.zip')

  try {
    run('ditto', ['-c', '-k', '--sequesterRsrc', '--keepParent', appPath, notaryZipPath], { dryRun })
    const submitOutput = run(
      'xcrun',
      [
        'notarytool',
        'submit',
        notaryZipPath,
        '--apple-id',
        env.MACOS_NOTARIZE_APPLE_ID,
        '--password',
        env.MACOS_NOTARIZE_APP_PASSWORD,
        '--team-id',
        env.MACOS_NOTARIZE_TEAM_ID,
        '--wait',
        '--output-format',
        'json',
      ],
      { capture: true, dryRun },
    )

    if (!dryRun) {
      const submission = JSON.parse(submitOutput)
      if (submission.status !== 'Accepted') {
        if (submission.id) {
          try {
            run(
              'xcrun',
              [
                'notarytool',
                'log',
                submission.id,
                '--apple-id',
                env.MACOS_NOTARIZE_APPLE_ID,
                '--password',
                env.MACOS_NOTARIZE_APP_PASSWORD,
                '--team-id',
                env.MACOS_NOTARIZE_TEAM_ID,
              ],
              { dryRun },
            )
          } catch {}
        }
        throw new Error(`Notarization status was '${submission.status}' (expected 'Accepted').`)
      }
    }

    run('xcrun', ['stapler', 'staple', appPath], { dryRun })
    run('xcrun', ['stapler', 'validate', appPath], { dryRun })
  } finally {
    if (!dryRun) {
      rmSync(tempRoot, { recursive: true, force: true })
    }
  }
}

function verifyPackagedMacosArtifact({ zipPath, signed, notarized, dryRun }) {
  const verifyDir = dryRun
    ? join(os.tmpdir(), 'iris-local-verify-dry-run')
    : mkdtempSync(join(os.tmpdir(), 'iris-local-verify-'))

  try {
    run('ditto', ['-x', '-k', zipPath, verifyDir], { dryRun })
    const appPath = findFirstFile(verifyDir, (entry) => entry.endsWith('.app'))
    if (!dryRun && !appPath) {
      throw new Error('Packaged zip did not contain a macOS .app bundle.')
    }

    if (signed) {
      run('codesign', ['--verify', '--deep', '--strict', '--verbose=2', appPath || '<macos-app-bundle>'], {
        dryRun,
      })
    }
    if (notarized) {
      run('spctl', ['--assess', '--type', 'execute', '--verbose=4', appPath || '<macos-app-bundle>'], {
        dryRun,
      })
    }
  } finally {
    if (!dryRun) {
      rmSync(verifyDir, { recursive: true, force: true })
    }
  }
}

function runWindowsPowerShell(vmName, script, { capture = false, dryRun = false } = {}) {
  return run(
    'prlctl',
    ['exec', vmName, '--current-user', 'powershell.exe', '-NoProfile', '-Command', script],
    { capture, dryRun },
  )
}

function shouldRunStep(step, options) {
  if (options.skipVerify && step === 'verify') {
    return false
  }
  if (options.only && !options.only.has(step)) {
    return false
  }
  if (options.skip.has(step)) {
    return false
  }
  return true
}

function syncRepoToWindowsVm({ vmName, sharedRepoPath, dryRun }) {
  const script = `
$sharedRepo = ${psQuote(sharedRepoPath)}
$guestRepo = Join-Path $env:USERPROFILE 'src\\iris-browser'
$guestRoot = Split-Path $guestRepo
New-Item -ItemType Directory -Force -Path $guestRoot | Out-Null
robocopy $sharedRepo $guestRepo /E /XD target dist node_modules .pnpm-store .git artifacts /XF .env.release.local .env.zapstore.local | Out-Null
if ($LASTEXITCODE -ge 8) { exit $LASTEXITCODE }
$binDir = Join-Path $env:USERPROFILE 'bin'
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
$shimPath = Join-Path $binDir 'pnpm.cmd'
$shimLines = @(
  '@echo off'
  'corepack pnpm %*'
)
Set-Content -Encoding ASCII -Path $shimPath -Value $shimLines
`

  runWindowsPowerShell(vmName, script, { dryRun })
}

function buildWindowsArtifacts({ env, assetPrefix, dryRun, builtLines }) {
  if (process.platform !== 'darwin') {
    throw new SkipStepError('Windows installer builds are only wired up for the macOS + Parallels workflow.')
  }
  if (!commandExists('prlctl')) {
    throw new SkipStepError('Skipping Windows installers because prlctl is unavailable.')
  }

  const sharedRepoPath = env.IRIS_WINDOWS_SHARED_REPO_PATH || defaultSharedWindowsRepoPath()
  if (!sharedRepoPath) {
    throw new SkipStepError('Skipping Windows installers because the shared repo path could not be derived; set IRIS_WINDOWS_SHARED_REPO_PATH.')
  }

  const vmName =
    env.IRIS_WINDOWS_VM_NAME ||
    autoDetectWindowsVmName(run('prlctl', ['list', '-a'], { capture: true, dryRun }))
  if (!vmName) {
    throw new SkipStepError('Skipping Windows installers because no unique running Windows VM was detected; set IRIS_WINDOWS_VM_NAME.')
  }

  ensureFrontendDistAvailable(dryRun)
  ensureDistDir(dryRun)
  syncRepoToWindowsVm({ vmName, sharedRepoPath, dryRun })

  const guiTargets = splitCsv(env.IRIS_WINDOWS_GUI_TARGETS || 'x86_64-pc-windows-msvc')
  const guestRepo = "(Join-Path $env:USERPROFILE 'src\\iris-browser')"
  const distPath = `${sharedRepoPath}\\dist\\iris-native`
  const pathSetup = [
    "$env:CI = 'true'",
    "$env:PATH = (Join-Path $env:USERPROFILE 'bin') + ';' + $env:PATH",
  ].join('\n')

  runWindowsPowerShell(
    vmName,
    `
${pathSetup}
Set-Location ${guestRepo}
${workspaceInstallCommands('corepack pnpm').join('\n')}
New-Item -ItemType Directory -Force -Path ${psQuote(distPath)} | Out-Null
`,
    { dryRun },
  )

  for (const target of guiTargets) {
    const arch = windowsArtifactArch(target)
    const installerName = `${assetPrefix}-windows-${arch}-setup.exe`
    runWindowsPowerShell(
      vmName,
      `
${pathSetup}
Set-Location ${guestRepo}
${windowsTauriBuildCommand(target)}
$bundleDir = Join-Path ${guestRepo} ${psQuote(`apps\\iris\\src-tauri\\target\\${target}\\release\\bundle\\nsis`)}
$installer = Get-ChildItem $bundleDir -Filter '*-setup.exe' | Select-Object -First 1
if (-not $installer) { throw ${psQuote(`No NSIS installer found for ${target}`)} }
Copy-Item $installer.FullName ${psQuote(`${distPath}\\${installerName}`)} -Force
`,
      { dryRun },
    )
    builtLines.push(`Built Windows ${arch} Iris NSIS installer inside Parallels VM ${vmName}.`)
  }
}

function buildMacosArtifacts({ env, pnpmInvocation, assetPrefix, dryRun, builtLines, allowUnsignedMacos }) {
  if (process.platform !== 'darwin' || process.arch !== 'arm64') {
    throw new SkipStepError('Skipping macOS app bundle because the host is not Apple Silicon macOS.')
  }

  ensureFrontendDistAvailable(dryRun)
  ensureDistDir(dryRun)
  const macosZipPath = join(distDir, `${assetPrefix}-macos-arm64.zip`)
  if (!dryRun) {
    rmSync(macosZipPath, { force: true })
  }

  const capabilities = detectLocalMacosReleaseCapabilities(env)
  if (!capabilities.signingReady && !allowUnsignedMacos) {
    const missing = capabilities.missingSigning.join(', ')
    throw new SkipStepError(
      `Skipping macOS app bundle because signing inputs are missing (${missing}). Pass --allow-unsigned-macos or set IRIS_ALLOW_UNSIGNED_MACOS=1 to force an unsigned zip.`,
    )
  }

  installFrontendDependencies(pnpmInvocation, { dryRun })
  runPnpm(
    pnpmInvocation,
    ['--dir', appDir, 'exec', 'tauri', 'build', '--config', packagingConfigPath(), '--target', 'aarch64-apple-darwin', '--bundles', 'app', '--no-sign', '--ci'],
    { dryRun },
  )

  const appPath = findBundleArtifact(
    appBundleCandidates('aarch64-apple-darwin'),
    'macos',
    (entry) => entry.endsWith('.app'),
  )
  if (!dryRun && !appPath) {
    throw new Error('No macOS .app bundle found in build output.')
  }

  const appPathForZip = appPath || '<macos-app-bundle>'
  let signed = false
  let notarized = false
  let signingContext = null
  try {
    if (capabilities.signingReady) {
      signingContext = prepareLocalMacosSigning(env, { dryRun })
      signLocalMacosApp({
        appPath: appPathForZip,
        env,
        keychainPath: signingContext.keychainPath,
        dryRun,
      })
      signed = true

      if (capabilities.notarizationReady) {
        notarizeLocalMacosApp({ appPath: appPathForZip, env, dryRun })
        notarized = true
      }
    }
  } finally {
    signingContext?.cleanup()
  }

  run(
    'ditto',
    ['-c', '-k', '--sequesterRsrc', '--keepParent', appPathForZip, macosZipPath],
    { dryRun },
  )

  verifyPackagedMacosArtifact({ zipPath: macosZipPath, signed, notarized, dryRun })

  if (notarized) {
    builtLines.push('Built signed and notarized Apple Silicon macOS Iris app locally.')
  } else if (signed) {
    const missing = capabilities.notarizationReady
      ? ''
      : ` Missing notarization inputs: ${capabilities.missingNotarization.join(', ')}.`
    builtLines.push(`Built signed Apple Silicon macOS Iris app locally without notarization.${missing}`)
  } else {
    builtLines.push('Built unsigned Apple Silicon macOS Iris app locally because unsigned output was explicitly allowed.')
  }
}

function buildLinuxArtifacts({ pnpmInvocation, env, assetPrefix, dryRun, builtLines }) {
  const target = 'x86_64-unknown-linux-gnu'
  const appImageDest = join(distDir, `${assetPrefix}-linux-x86_64.AppImage`)
  const debDest = join(distDir, `${assetPrefix}-linux-x86_64.deb`)

  ensureFrontendDistAvailable(dryRun)
  ensureDistDir(dryRun)

  if (process.platform === 'linux') {
    installFrontendDependencies(pnpmInvocation, { dryRun })
    runPnpm(
      pnpmInvocation,
      ['--dir', appDir, 'exec', 'tauri', 'build', '--config', packagingConfigPath(), '--target', target, '--bundles', 'appimage,deb', '--ci'],
      { dryRun },
    )
  } else {
    if (!commandExists('docker')) {
      throw new SkipStepError('Skipping Linux bundles because docker is unavailable.')
    }

    const imageName = env.IRIS_RELEASE_DOCKER_IMAGE || 'hashtree/iris-native-linux-release'
    const platform = env.IRIS_RELEASE_DOCKER_PLATFORM || 'linux/amd64'
    const command = linuxDockerShellCommand(target)

    run('docker', ['build', '--platform', platform, '-f', dockerfile, '-t', imageName, dirname(dockerfile)], { dryRun })
    run(
      'docker',
      [
        'run',
        '--rm',
        '--platform',
        platform,
        '-e',
        'CI=true',
        ...linuxDockerVolumeMounts(repoRoot).flatMap((mount) => ['-v', mount]),
        '-w',
        '/workspace',
        imageName,
        'bash',
        '-lc',
        command,
      ],
      { dryRun },
    )
  }

  const appImagePath = findBundleArtifact(
    appBundleCandidates(target),
    'appimage',
    (entry) => entry.endsWith('.AppImage'),
  )
  const debPath = findBundleArtifact(
    appBundleCandidates(target),
    'deb',
    (entry) => entry.endsWith('.deb'),
  )

  if (!dryRun && !appImagePath) {
    throw new Error('No Linux AppImage bundle found in build output.')
  }
  if (!dryRun && !debPath) {
    throw new Error('No Linux .deb bundle found in build output.')
  }

  if (!dryRun) {
    copyFileSync(appImagePath, appImageDest)
    copyFileSync(debPath, debDest)
  }

  builtLines.push(process.platform === 'linux'
    ? 'Built Linux Iris AppImage and .deb locally.'
    : 'Built Linux Iris AppImage and .deb through Docker.')
}

function runVerify({ pnpmInvocation, dryRun, builtLines }) {
  installFrontendDependencies(pnpmInvocation, { dryRun })
  runPnpm(pnpmInvocation, ['--dir', appDir, 'build'], { dryRun })
  runPnpm(pnpmInvocation, ['--dir', appDir, 'run', 'test:icons'], { dryRun })
  builtLines.push('Ran pnpm build and test:icons for Iris.')
}

export function collectReleaseAssetPaths(assetPrefix, outputDir = distDir) {
  if (!existsSync(outputDir)) {
    return []
  }

  return readdirSync(outputDir)
    .sort()
    .map((entry) => join(outputDir, entry))
    .filter((fullPath) => statSync(fullPath).isFile())
    .filter((fullPath) => basename(fullPath).startsWith(`${assetPrefix}-`))
    .filter((fullPath) => !fullPath.endsWith('.sha256'))
}

function stageRelease({ tag, commit, stageDir, outputDir, assetPrefix, builtLines, skippedLines, dryRun }) {
  const assetPaths = collectReleaseAssetPaths(assetPrefix, outputDir)
  if (dryRun) {
    console.log(`Would stage ${assetPaths.length} currently visible asset(s) into ${stageDir}`)
    return { assetPaths, stageDir }
  }

  if (assetPaths.length === 0) {
    throw new Error(`No Iris release assets found for ${tag} in ${outputDir}.`)
  }

  rmSync(stageDir, { recursive: true, force: true })
  mkdirSync(join(stageDir, 'assets'), { recursive: true })

  const stagedAssetPaths = []
  for (const assetPath of assetPaths) {
    const stagedPath = join(stageDir, 'assets', basename(assetPath))
    copyFileSync(assetPath, stagedPath)
    stagedAssetPaths.push(stagedPath)
  }

  const createdAt = Math.floor(Date.now() / 1000)
  const manifest = buildReleaseManifest({
    tag,
    commit,
    createdAt,
    assetPaths: stagedAssetPaths,
  })

  writeFileSync(join(stageDir, 'release.json'), `${JSON.stringify(manifest, null, 2)}\n`)
  writeFileSync(
    join(stageDir, 'notes.md'),
    renderReleaseNotes({
      tag,
      commit,
      assetNames: stagedAssetPaths.map((assetPath) => basename(assetPath)),
      builtLines,
      skippedLines,
    }),
  )

  return { assetPaths, stageDir }
}

function publishRelease({ stageDir, releaseTree, tag, dryRun }) {
  if (!releaseTree) {
    throw new Error('--publish requires an explicit --release-tree')
  }

  if (dryRun) {
    console.log(`Would publish ${tag} from ${stageDir} into ${releaseTree}`)
    return 'dry-run'
  }

  const addOutput = run('htree', ['add', stageDir], { capture: true, dryRun })
  const match = addOutput.match(/^\s*url:\s*(\S+)/m)
  if (!match) {
    throw new Error('Could not parse htree add output for release CID.')
  }

  const cid = match[1]
  run('htree', ['release', 'publish', releaseTree, tag, cid], { dryRun })
  return cid
}

function isMainModule() {
  if (!process.argv[1]) {
    return false
  }
  return resolve(process.argv[1]) === fileURLToPath(import.meta.url)
}

function main() {
  const options = parseArgs(process.argv.slice(2))
  if (options.help) {
    console.log(usage())
    return
  }

  const { loaded, loadedPaths } = readOptionalEnvFiles([...defaultEnvFiles, ...options.envFiles])
  const env = { ...loaded, ...process.env }
  const tag = options.tag || readCargoPackageVersionTag(readFileSync(appCargoToml, 'utf8'))
  const assetPrefix = assetPrefixForTag(tag)
  const stageDir =
    options.stageDir || join(os.tmpdir(), `iris-release-${tag.replace(/[^\w.-]/g, '_')}`)

  const builtLines = []
  const skippedLines = []
  const failures = []

  console.log(`Release tag: ${tag}`)
  console.log(`Asset prefix: ${assetPrefix}`)
  console.log(`Output dir: ${distDir}`)
  if (options.releaseTree) {
    console.log(`Release tree: ${options.releaseTree}`)
  }
  if (loadedPaths.length > 0) {
    console.log(`Loaded env files: ${loadedPaths.join(', ')}`)
  }
  if (options.dryRun) {
    console.log('Dry run mode: no build, copy, or publish commands will be executed.')
  }

  const allowUnsignedMacos = options.allowUnsignedMacos || envFlagEnabled(env.IRIS_ALLOW_UNSIGNED_MACOS)
  const pnpmInvocation = resolveHostPnpmInvocation()
  const steps = [
    ['verify', () => runVerify({ pnpmInvocation, dryRun: options.dryRun, builtLines })],
    ['macos', () => buildMacosArtifacts({
      env,
      pnpmInvocation,
      assetPrefix,
      dryRun: options.dryRun,
      builtLines,
      allowUnsignedMacos,
    })],
    ['linux', () => buildLinuxArtifacts({ pnpmInvocation, env, assetPrefix, dryRun: options.dryRun, builtLines })],
    ['windows', () => buildWindowsArtifacts({ env, assetPrefix, dryRun: options.dryRun, builtLines })],
  ]

  for (const [name, fn] of steps) {
    if (!shouldRunStep(name, options)) {
      skippedLines.push(`${name} skipped by CLI options.`)
      continue
    }

    try {
      fn()
    } catch (error) {
      if (error instanceof SkipStepError) {
        skippedLines.push(error.message)
        continue
      }
      if (name === 'verify') {
        throw error
      }
      const message = `${name} build failed: ${error.message}`
      skippedLines.push(message)
      failures.push(message)
    }
  }

  const commit = run('git', ['rev-parse', 'HEAD'], { capture: true, dryRun: options.dryRun }) || 'HEAD'
  stageRelease({
    tag,
    commit,
    stageDir,
    outputDir: distDir,
    assetPrefix,
    builtLines,
    skippedLines,
    dryRun: options.dryRun,
  })

  if (failures.length > 0) {
    throw new Error(failures.join('; '))
  }

  if (options.publish) {
    if (!commandExists('htree')) {
      throw new Error('Missing htree; cannot publish release.')
    }
    const cid = publishRelease({
      stageDir,
      releaseTree: options.releaseTree,
      tag,
      dryRun: options.dryRun,
    })
    console.log(`Published ${tag} to ${options.releaseTree} via ${cid}`)
  } else {
    console.log(`Staged release at ${stageDir}`)
  }
}

if (isMainModule()) {
  try {
    main()
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error))
    process.exit(1)
  }
}
