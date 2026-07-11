export type SuggestionKind = 'history' | 'bookmark' | 'tab' | 'search'

export interface Suggestion {
  kind: SuggestionKind
  title: string
  url: string
  score: number
}
