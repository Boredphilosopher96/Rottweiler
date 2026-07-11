import type { CollisionStrategy } from '@shared/downloads'
// Pure filename logic for downloads. No fs here - existence is injected so this
// is unit-testable. Covers the "little things": collision naming (user's choice
// of classic increment vs timestamp), sanitization, and danger detection.

const BAD_CHARS = /[/\\:*?"<>|\u0000-\u001f]/g

const DANGEROUS_EXT = new Set([
  '.exe', '.msi', '.bat', '.cmd', '.com', '.scr', '.pif', '.ps1', '.vbs', '.js',
  '.jar', '.apk', '.dmg', '.pkg', '.deb', '.rpm', '.app', '.sh', '.run', '.bin'
])

export function sanitizeFilename(name: string): string {
  const clean = name.replace(BAD_CHARS, '_').replace(/^\.+/, '').trim()
  return clean.length > 0 ? clean : 'download'
}

export function splitName(filename: string): { stem: string; ext: string } {
  const dot = filename.lastIndexOf('.')
  if (dot <= 0) return { stem: filename, ext: '' }
  return { stem: filename.slice(0, dot), ext: filename.slice(dot) }
}

export function isDangerous(filename: string): boolean {
  return DANGEROUS_EXT.has(splitName(filename).ext.toLowerCase())
}

function pad2(n: number): string {
  return n < 10 ? '0' + n : String(n)
}

/**
 * Resolve a non-colliding filename given a strategy.
 *  - 'overwrite'  → the name as-is
 *  - 'increment'  → name-01.ext, name-02.ext, ...   (the classic, zero-padded)
 *  - 'timestamp'  → name-HH-MM-SS.ext               (e.g. report-09-48-12.pdf)
 * `exists(name)` decides collisions; `now` is injectable for testing.
 */
export function resolveFilename(
  filename: string,
  strategy: CollisionStrategy,
  exists: (name: string) => boolean,
  now: Date = new Date()
): string {
  if (strategy === 'overwrite') return filename
  if (!exists(filename)) return filename

  const { stem, ext } = splitName(filename)

  if (strategy === 'timestamp') {
    const t = `${pad2(now.getHours())}-${pad2(now.getMinutes())}-${pad2(now.getSeconds())}`
    let candidate = `${stem}-${t}${ext}`
    let i = 1
    while (exists(candidate)) {
      candidate = `${stem}-${t}-${pad2(i)}${ext}`
      i++
    }
    return candidate
  }

  // increment
  for (let i = 1; i < 1000; i++) {
    const candidate = `${stem}-${pad2(i)}${ext}`
    if (!exists(candidate)) return candidate
  }
  // absurd fallback
  return `${stem}-${Date.now()}${ext}`
}
