import { statSync } from 'node:fs'
import { basename } from 'node:path'

export function parseEnvFile(text) {
  const values = {}
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim()
    if (!line || line.startsWith('#')) {
      continue
    }

    const separator = line.indexOf('=')
    if (separator <= 0) {
      continue
    }

    const key = line.slice(0, separator).trim()
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) {
      continue
    }

    let value = line.slice(separator + 1).trim()
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1)
    }

    value = value
      .replace(/\\n/g, '\n')
      .replace(/\\r/g, '\r')
      .replace(/\\t/g, '\t')

    values[key] = value
  }

  return values
}

export function splitCsv(value) {
  return (value || '')
    .split(',')
    .map((part) => part.trim())
    .filter(Boolean)
}

export function normalizeTag(value) {
  if (!value || !value.trim()) {
    throw new Error('Release tag must not be empty')
  }

  return value.startsWith('v') ? value : `v${value}`
}

export function assetPrefixForTag(tag) {
  return `iris-${normalizeTag(tag)}`
}

export function readCargoPackageVersionTag(cargoTomlText) {
  const match = cargoTomlText.match(/^\[package\][\s\S]*?^version\s*=\s*"([^"\n]+)"/m)
  if (!match) {
    throw new Error('Could not find [package] version in Cargo.toml')
  }

  return normalizeTag(match[1])
}

export function autoDetectWindowsVmName(prlctlListOutput) {
  const candidates = []
  for (const line of prlctlListOutput.split(/\r?\n/)) {
    const trimmed = line.trim()
    if (!trimmed.startsWith('{')) {
      continue
    }

    const match = trimmed.match(/^\{[^}]+\}\s+(\S+)\s+\S+\s+(.+)$/)
    if (!match) {
      continue
    }

    const status = match[1].toLowerCase()
    const name = match[2].trim()
    if ((status === 'running' || status === 'suspended') && /windows/i.test(name)) {
      candidates.push(name)
    }
  }

  return candidates.length === 1 ? candidates[0] : null
}

export function describeAsset(name) {
  if (/^iris-v.*-macos-arm64\.zip$/.test(name)) {
    return 'macOS Apple Silicon app'
  }
  if (/^iris-v.*-linux-x86_64\.AppImage$/.test(name)) {
    return 'Linux x64 AppImage'
  }
  if (/^iris-v.*-linux-x86_64\.deb$/.test(name)) {
    return 'Linux x64 Debian package'
  }
  if (/^iris-v.*-windows-x64-setup\.exe$/.test(name)) {
    return 'Windows x64 installer'
  }
  if (/^iris-v.*-windows-arm64-setup\.exe$/.test(name)) {
    return 'Windows ARM64 installer'
  }

  return name
}

function findAsset(assetNames, pattern) {
  return [...assetNames].sort((left, right) => left.localeCompare(right)).find((name) => pattern.test(name))
}

export function buildReleaseManifest({ tag, commit, createdAt, assetPaths }) {
  const normalizedTag = normalizeTag(tag)
  const assets = [...assetPaths]
    .map((assetPath) => ({
      name: basename(assetPath),
      path: `assets/${basename(assetPath)}`,
      size: statSync(assetPath).size,
    }))
    .sort((left, right) => left.name.localeCompare(right.name))

  return {
    id: normalizedTag,
    title: normalizedTag,
    tag: normalizedTag,
    commit,
    created_at: createdAt,
    published_at: createdAt,
    draft: false,
    prerelease: normalizedTag.includes('-'),
    notes_file: 'notes.md',
    assets,
  }
}

export function renderReleaseNotes({
  tag,
  commit,
  assetNames,
  builtLines = [],
  skippedLines = [],
}) {
  const normalizedTag = normalizeTag(tag)
  const sortedAssetNames = [...assetNames].sort((left, right) => left.localeCompare(right))
  const macosArm64 = findAsset(sortedAssetNames, /^iris-v.*-macos-arm64\.zip$/)
  const linuxAppImage = findAsset(sortedAssetNames, /^iris-v.*-linux-x86_64\.AppImage$/)
  const linuxDeb = findAsset(sortedAssetNames, /^iris-v.*-linux-x86_64\.deb$/)
  const windowsX64 = findAsset(sortedAssetNames, /^iris-v.*-windows-x64-setup\.exe$/)
  const windowsArm64 = findAsset(sortedAssetNames, /^iris-v.*-windows-arm64-setup\.exe$/)

  const lines = []

  if (macosArm64 || linuxAppImage || linuxDeb || windowsX64 || windowsArm64) {
    lines.push('## Installation', '')

    if (macosArm64) {
      lines.push('### macOS', '')
      lines.push(`- Download \`${macosArm64}\`, unzip it, and move \`Iris.app\` into \`/Applications\`.`)
      lines.push('')
    }

    if (linuxAppImage || linuxDeb) {
      lines.push('### Linux', '')
      if (linuxAppImage) {
        lines.push(`- Run \`chmod +x ${linuxAppImage} && ./${linuxAppImage}\`.`)
      }
      if (linuxDeb) {
        lines.push(`- Install \`${linuxDeb}\` with \`sudo apt install ./${linuxDeb}\`.`)
      }
      lines.push('')
    }

    if (windowsX64 || windowsArm64) {
      lines.push('### Windows', '')
      if (windowsX64) {
        lines.push(`- Run \`${windowsX64}\`.`)
      }
      if (windowsArm64) {
        lines.push(`- Run \`${windowsArm64}\`.`)
      }
      lines.push('')
    }
  }

  lines.push('## Downloads', '')

  for (const name of sortedAssetNames) {
    lines.push(`- ${describeAsset(name)}: \`${name}\``)
  }

  lines.push('', '## Build Info', '', `- Release \`${normalizedTag}\` from commit \`${commit}\`.`)

  for (const line of builtLines) {
    lines.push(`- ${line}`)
  }

  if (skippedLines.length > 0) {
    lines.push('', '## Skipped or Not Built', '')
    for (const line of skippedLines) {
      lines.push(`- ${line}`)
    }
  }

  return `${lines.join('\n')}\n`
}
