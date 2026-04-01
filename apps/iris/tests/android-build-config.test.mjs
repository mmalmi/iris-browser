import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'

const appRoot = new URL('..', import.meta.url)
const packageJsonPath = join(appRoot.pathname, 'package.json')
const gradlePropertiesPath = join(appRoot.pathname, 'src-tauri/gen/android/gradle.properties')
const rustPluginPath = join(
  appRoot.pathname,
  'src-tauri/gen/android/buildSrc/src/main/java/to/iris/browser/kotlin/RustPlugin.kt',
)
const appBuildGradlePath = join(appRoot.pathname, 'src-tauri/gen/android/app/build.gradle.kts')
const androidManifestPath = join(appRoot.pathname, 'src-tauri/gen/android/app/src/main/AndroidManifest.xml')
const releaseNetworkSecurityConfigPath = join(
  appRoot.pathname,
  'src-tauri/gen/android/app/src/main/res/xml/network_security_config.xml',
)
const debugNetworkSecurityConfigPath = join(
  appRoot.pathname,
  'src-tauri/gen/android/app/src/debug/res/xml/network_security_config.xml',
)

test('android gradle defaults only target 64-bit arm builds', () => {
  const gradleProperties = readFileSync(gradlePropertiesPath, 'utf8')

  assert.match(gradleProperties, /^targetList=aarch64$/m)
  assert.match(gradleProperties, /^archList=arm64$/m)
  assert.match(gradleProperties, /^abiList=arm64-v8a$/m)
  assert.doesNotMatch(gradleProperties, /armv7|armeabi-v7a|i686|x86_64/)
})

test('android rust plugin derives flavors from configured ABI lists', () => {
  const rustPlugin = readFileSync(rustPluginPath, 'utf8')

  assert.match(rustPlugin, /val defaultAbiList = listOf\("arm64-v8a"\)/)
  assert.match(rustPlugin, /val defaultArchList = listOf\("arm64"\)/)
  assert.match(rustPlugin, /listOf\("aarch64"\)/)
  assert.match(rustPlugin, /archList\.forEachIndexed/)
  assert.match(rustPlugin, /abiFilters\.add\(abiList\[index\]\)/)
  assert.doesNotMatch(rustPlugin, /armeabi-v7a|armv7|i686|x86_64/)
})

test('android app gradle config keeps debug symbols only for arm64', () => {
  const appBuildGradle = readFileSync(appBuildGradlePath, 'utf8')

  assert.match(appBuildGradle, /arm64-v8a/)
  assert.doesNotMatch(appBuildGradle, /armeabi-v7a|x86|x86_64/)
})

test('android release only allows cleartext to Iris loopback hosts', () => {
  const androidManifest = readFileSync(androidManifestPath, 'utf8')
  const networkSecurityConfig = readFileSync(releaseNetworkSecurityConfigPath, 'utf8')

  assert.match(androidManifest, /android:networkSecurityConfig="@xml\/network_security_config"/)
  assert.doesNotMatch(androidManifest, /usesCleartextTraffic/)
  assert.match(networkSecurityConfig, /<base-config cleartextTrafficPermitted="false">/)
  assert.match(networkSecurityConfig, /<domain includeSubdomains="true">htree\.localhost<\/domain>/)
  assert.match(networkSecurityConfig, /<domain includeSubdomains="true">localhost<\/domain>/)
  assert.match(networkSecurityConfig, /<domain includeSubdomains="true">127\.0\.0\.1<\/domain>/)
})

test('android debug network security config keeps general cleartext enabled for dev', () => {
  const networkSecurityConfig = readFileSync(debugNetworkSecurityConfigPath, 'utf8')

  assert.match(networkSecurityConfig, /<base-config cleartextTrafficPermitted="true">/)
})

test('repo android build script hardcodes the arm64-only target', () => {
  const packageJson = JSON.parse(readFileSync(packageJsonPath, 'utf8'))

  assert.equal(
    packageJson.scripts['tauri:build:android'],
    'tauri android build -t aarch64 --apk true --aab false --ci',
  )
})
