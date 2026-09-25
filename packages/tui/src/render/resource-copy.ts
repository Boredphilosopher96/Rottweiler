/** Actionable copy for bounded client operations; diagnostics keep their original errors. */
export const DRAFT_LIMIT_NOTICE = "Couldn't add more content. Shorten this draft or remove an attachment, then try again."
export const DRAFT_SWITCH_LIMIT_NOTICE = "Couldn't switch agents while keeping this draft. Shorten it or remove an attachment, then try again."
export const PANEL_LIMIT_NOTICE = "Couldn't load this panel. Close open content views, then reopen the panel."
export const CHILD_LIST_LIMIT_NOTICE = "Too many child agents to show. Close finished agents, then reopen the agent list."

export function resourceLimitCopy(message: string | undefined): string | null {
  switch (message) {
    case DRAFT_LIMIT_NOTICE: case DRAFT_SWITCH_LIMIT_NOTICE: case PANEL_LIMIT_NOTICE: case CHILD_LIST_LIMIT_NOTICE: return message
    case "Draft storage is full. Shorten a draft or remove an attachment before adding more content.": return DRAFT_LIMIT_NOTICE
    case "Draft storage is full. Shorten a draft or remove an attachment before switching.": return DRAFT_SWITCH_LIMIT_NOTICE
    case "UI content cache is full with active readers.": return PANEL_LIMIT_NOTICE
    case "child catalog exceeds its admitted actor count": return CHILD_LIST_LIMIT_NOTICE
    default: return null
  }
}
