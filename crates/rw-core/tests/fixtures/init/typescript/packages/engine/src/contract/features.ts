/** UI-facing description of an optional/experimental feature and whether it's on. */
export interface FeatureInfo {
  id: string
  name: string
  description: string
  experimental: boolean
  enabled: boolean
}
