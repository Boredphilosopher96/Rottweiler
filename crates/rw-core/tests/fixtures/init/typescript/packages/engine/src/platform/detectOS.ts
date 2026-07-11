import * as os from 'node:os'
import type { OSFamily, OSInfo, OSEnv } from '../contract/platform'

const BSD_PLATFORMS = new Set(['freebsd', 'openbsd', 'netbsd', 'dragonfly'])

/** Coarse family from process.platform, with a kernel-string fallback for exotic runtimes. */
export function classifyFamily(platform: string, kernelLower: string): OSFamily {
  switch (platform) {
    case 'win32':
    case 'cygwin':
    case 'msys':
      return 'windows'
    case 'darwin':
      return 'macos'
    case 'linux':
    case 'android':
      return 'linux'
    case 'freebsd':
    case 'openbsd':
    case 'netbsd':
    case 'dragonfly':
      return 'bsd'
    default:
      if (kernelLower.includes('windows')) return 'windows'
      if (kernelLower.includes('darwin')) return 'macos'
      if (kernelLower.includes('bsd')) return 'bsd'
      if (kernelLower.includes('linux')) return 'linux'
      return 'unknown'
  }
}

/** Finer variant: android / wsl / specific BSD, or null when nothing distinctive. */
export function detectVariant(platform: string, releaseLower: string, kernelLower: string): string | null {
  if (platform === 'android' || releaseLower.includes('android')) return 'android'
  if (platform === 'linux' && (releaseLower.includes('microsoft') || releaseLower.includes('wsl'))) return 'wsl'
  if (BSD_PLATFORMS.has(platform)) return platform
  if (kernelLower.includes('freebsd')) return 'freebsd'
  if (kernelLower.includes('openbsd')) return 'openbsd'
  if (kernelLower.includes('netbsd')) return 'netbsd'
  if (kernelLower.includes('dragonfly')) return 'dragonfly'
  return null
}

/** Pure: classify a supplied environment. This is the unit-testable core. */
export function detectOSFrom(env: OSEnv): OSInfo {
  const platform = env.platform || 'unknown'
  const kernel = env.type || ''
  const kernelLower = kernel.toLowerCase()
  const releaseLower = (env.release || '').toLowerCase()
  const family = classifyFamily(platform, kernelLower)
  return {
    family,
    platform,
    variant: detectVariant(platform, releaseLower, kernelLower),
    arch: env.arch || 'unknown',
    release: env.release || '',
    kernel,
    isWindows: family === 'windows',
    isMac: family === 'macos',
    isLinux: family === 'linux',
    isBSD: family === 'bsd'
  }
}

function safe<T>(f: () => T): T | undefined {
  try { return f() } catch { return undefined }
}

/** Detect from the live Node/Electron runtime. */
export function detectOS(): OSInfo {
  const proc = (globalThis as { process?: { platform?: string; arch?: string } }).process ?? {}
  return detectOSFrom({
    platform: typeof proc.platform === 'string' ? proc.platform : 'unknown',
    arch: typeof proc.arch === 'string' ? proc.arch : 'unknown',
    release: safe(() => os.release()) ?? '',
    type: safe(() => os.type()) ?? ''
  })
}
