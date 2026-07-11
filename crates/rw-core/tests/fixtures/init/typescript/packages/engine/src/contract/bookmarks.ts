export interface BookmarkNode {
  id: string
  type: 'bookmark' | 'folder'
  title: string
  /** present for bookmarks, absent for folders. */
  url?: string
  parentId: string | null
  createdAt: number
}

export interface BookmarkTree {
  /** flat list; tree is derived from parentId. rootId is a folder with parentId null. */
  nodes: BookmarkNode[]
  rootId: string
}
