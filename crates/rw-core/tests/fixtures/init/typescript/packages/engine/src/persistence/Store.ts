import { join } from 'node:path'
import { mkdirSync, readFileSync, writeFileSync, renameSync, existsSync } from 'node:fs'

/**
 * Dead-simple durable JSON store, rooted at an explicit directory (so each
 * profile owns its own files). Atomic writes via temp-file + rename so a crash
 * mid-write never corrupts the file. History (unbounded) moves to SQLite later;
 * this is deliberately not that.
 */
export class Store<T extends object> {
  private readonly file: string
  private data: T

  constructor(dir: string, name: string, defaults: T) {
    mkdirSync(dir, { recursive: true })
    this.file = join(dir, `${name}.json`)
    this.data = this.load(defaults)
  }

  private load(defaults: T): T {
    try {
      if (existsSync(this.file)) {
        return { ...defaults, ...(JSON.parse(readFileSync(this.file, 'utf-8')) as T) }
      }
    } catch {
      // corrupt/unreadable → defaults rather than crash
    }
    return { ...defaults }
  }

  get(): T {
    return this.data
  }

  set(patch: Partial<T>): void {
    this.data = { ...this.data, ...patch }
    this.flush()
  }

  replace(next: T): void {
    this.data = next
    this.flush()
  }

  private flush(): void {
    const tmp = `${this.file}.tmp`
    writeFileSync(tmp, JSON.stringify(this.data, null, 2), 'utf-8')
    renameSync(tmp, this.file)
  }
}
