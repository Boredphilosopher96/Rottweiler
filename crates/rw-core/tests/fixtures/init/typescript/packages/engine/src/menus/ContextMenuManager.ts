import { CTX, type ContextMenuItem, type ContextMenuModel } from '@shared/menus'

/** The right-click context the engine needs, decoupled from Electron's type so it's testable. */
export interface ContextInput {
  x: number
  y: number
  linkURL: string
  srcURL: string
  selectionText: string
  isEditable: boolean
  mediaType: string
  misspelledWord: string
  dictionarySuggestions: string[]
  editFlags: { canCut: boolean; canCopy: boolean; canPaste: boolean; canSelectAll: boolean }
}

export interface NavFlags {
  canGoBack: boolean
  canGoForward: boolean
}

/**
 * Builds a structured context menu from a right-click. Pure and UI-agnostic: the
 * shell renders the returned model and reports back the chosen action id, which the
 * Browser executes. Sections are separated by relevance (link, image, selection,
 * editable field, page) the way desktop browsers group them.
 */
export class ContextMenuManager {
  build(tabId: string, input: ContextInput, nav: NavFlags): ContextMenuModel {
    const items: ContextMenuItem[] = []
    const add = (id: ContextMenuItem['id'], label: string, enabled = true): void => {
      items.push({ id, label, enabled })
    }

    if (input.linkURL) {
      add(CTX.OpenLinkNewTab, 'Open link in new tab')
      add(CTX.CopyLink, 'Copy link address')
      add(CTX.SaveLinkAs, 'Save link as…')
    }
    if (input.mediaType === 'image' && input.srcURL) {
      add(CTX.OpenImageNewTab, 'Open image in new tab')
      add(CTX.CopyImage, 'Copy image')
      add(CTX.SaveImageAs, 'Save image as…')
    }
    const replacements = input.misspelledWord ? input.dictionarySuggestions.slice(0, 5) : []
    if (input.misspelledWord) add(CTX.AddToDictionary, 'Add to dictionary')

    if (input.isEditable) {
      add(CTX.Cut, 'Cut', input.editFlags.canCut)
      add(CTX.Copy, 'Copy', input.editFlags.canCopy)
      add(CTX.Paste, 'Paste', input.editFlags.canPaste)
      add(CTX.SelectAll, 'Select all', input.editFlags.canSelectAll)
    } else if (input.selectionText) {
      add(CTX.Copy, 'Copy')
      add(CTX.SearchSelection, `Search for “${truncate(input.selectionText, 24)}”`)
    }

    // page section always available
    add(CTX.Back, 'Back', nav.canGoBack)
    add(CTX.Forward, 'Forward', nav.canGoForward)
    add(CTX.Reload, 'Reload')
    add(CTX.ViewSource, 'View source')
    add(CTX.Inspect, 'Inspect element')

    return {
      tabId,
      x: input.x,
      y: input.y,
      items,
      linkURL: input.linkURL,
      srcURL: input.srcURL,
      selectionText: input.selectionText,
      misspelledWord: input.misspelledWord,
      replacements
    }
  }
}

function truncate(s: string, n: number): string {
  const t = s.trim().replace(/\s+/g, ' ')
  return t.length > n ? t.slice(0, n) + '…' : t
}
