import { EventEmitter } from 'node:events'
import { randomUUID } from 'node:crypto'
import { Store } from '../persistence/Store'
import { importNetscape } from './importNetscape'
import type { BookmarkNode } from '@shared/bookmarks'

interface BookmarksData {
  nodes: BookmarkNode[]
  rootId: string
}

/** Persisted bookmark tree (flat + parentId) for one profile. Emits 'changed'. */
export class BookmarkManager extends EventEmitter {
  private store: Store<BookmarksData>

  constructor(dir: string) {
    super()
    this.store = new Store<BookmarksData>(dir, 'bookmarks', { nodes: [], rootId: '' })
    if (!this.store.get().rootId) {
      const root: BookmarkNode = {
        id: randomUUID(),
        type: 'folder',
        title: 'Bookmarks',
        parentId: null,
        createdAt: Date.now()
      }
      this.store.replace({ nodes: [root], rootId: root.id })
    }
  }

  get rootId(): string {
    return this.store.get().rootId
  }

  get nodes(): BookmarkNode[] {
    return this.store.get().nodes
  }

  add(title: string, url: string, parentId: string | null = null): BookmarkNode {
    const d = this.store.get()
    const node: BookmarkNode = {
      id: randomUUID(),
      type: 'bookmark',
      title: title || url,
      url,
      parentId: parentId ?? d.rootId,
      createdAt: Date.now()
    }
    this.store.replace({ nodes: [...d.nodes, node], rootId: d.rootId })
    this.emit('changed')
    return node
  }

  remove(id: string): void {
    const d = this.store.get()
    const toRemove = new Set<string>([id])
    let grew = true
    while (grew) {
      grew = false
      for (const n of d.nodes) {
        if (n.parentId && toRemove.has(n.parentId) && !toRemove.has(n.id)) {
          toRemove.add(n.id)
          grew = true
        }
      }
    }
    this.store.replace({ nodes: d.nodes.filter((n) => !toRemove.has(n.id)), rootId: d.rootId })
    this.emit('changed')
  }

  importHtml(html: string): number {
    const d = this.store.get()
    const imported = importNetscape(html, d.rootId)
    this.store.replace({ nodes: [...d.nodes, ...imported], rootId: d.rootId })
    this.emit('changed')
    return imported.filter((n) => n.type === 'bookmark').length
  }

  replaceAll(nodes: BookmarkNode[]): void {
    const rootId = nodes.find((n) => n.parentId === null)?.id ?? this.store.get().rootId
    this.store.replace({ nodes, rootId })
    this.emit('changed')
  }
}
