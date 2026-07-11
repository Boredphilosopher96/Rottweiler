export interface ZoomLevel {
  origin: string
  factor: number
}

/** Discrete zoom stops, mirroring what desktop browsers use. */
export const ZOOM_LEVELS = [0.5, 0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2, 2.5, 3] as const

/** Pure: next stop up (dir=1) or down (dir=-1) from the current factor. */
export function zoomStep(current: number, dir: 1 | -1): number {
  const levels = ZOOM_LEVELS as readonly number[]
  // index of the nearest existing stop
  let nearest = 0
  for (let i = 1; i < levels.length; i++) {
    if (Math.abs(levels[i] - current) < Math.abs(levels[nearest] - current)) nearest = i
  }
  const next = Math.min(levels.length - 1, Math.max(0, nearest + dir))
  return levels[next]
}
