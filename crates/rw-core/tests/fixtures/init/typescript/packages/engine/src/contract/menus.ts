/** Action ids a context-menu item can carry. The engine executes these; the UI renders. */
export const CTX = {
  Back: 'ctx.back',
  Forward: 'ctx.forward',
  Reload: 'ctx.reload',
  OpenLinkNewTab: 'ctx.openLinkNewTab',
  CopyLink: 'ctx.copyLink',
  SaveLinkAs: 'ctx.saveLinkAs',
  OpenImageNewTab: 'ctx.openImageNewTab',
  CopyImage: 'ctx.copyImage',
  SaveImageAs: 'ctx.saveImageAs',
  Copy: 'ctx.copy',
  Cut: 'ctx.cut',
  Paste: 'ctx.paste',
  SelectAll: 'ctx.selectAll',
  SearchSelection: 'ctx.searchSelection',
  ViewSource: 'ctx.viewSource',
  Inspect: 'ctx.inspect',
  AddToDictionary: 'ctx.addToDictionary'
} as const

export type ContextActionId = (typeof CTX)[keyof typeof CTX]

export interface ContextMenuItem {
  id: ContextActionId
  label: string
  enabled: boolean
}

/** A menu the engine built from a right-click; the UI renders it and reports the chosen id. */
export interface ContextMenuModel {
  tabId: string
  x: number
  y: number
  items: ContextMenuItem[]
  linkURL: string
  srcURL: string
  selectionText: string
  misspelledWord: string
  replacements: string[]
}
