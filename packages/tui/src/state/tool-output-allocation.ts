import { type ToolOutputBuffer, type ToolOutputNode } from "./display-buffer"

/** A transaction owns source heads, so rejected branches never change retained stream accounting. */
export class ToolOutputAllocation {
  readonly #heads: Set<ToolOutputBuffer>
  #cover: ToolOutputNode | null
  #linear: boolean
  #bytes: number
  constructor(previous: ToolOutputAllocation | undefined, stage: (bytes: number) => void) {
    stage(128 + (previous === undefined ? 0 : previous.#heads.size * 32))
    this.#heads = new Set(previous === undefined ? [] : previous.#heads)
    this.#cover = previous === undefined ? null : previous.#cover
    this.#linear = previous === undefined ? true : previous.#linear
    this.#bytes = previous === undefined ? 0 : previous.#bytes
  }
  get bytes(): number { return this.#bytes }
  get empty(): boolean { return this.#heads.size === 0 }
  add(buffer: ToolOutputBuffer, stage: (bytes: number) => void): void {
    if (this.#heads.has(buffer)) throw new Error("duplicate source head owner")
    stage(32)
    this.#heads.add(buffer)
    const node = buffer.allocationNode
    if (node === null) return
    if (this.#linear && this.#cover === null) { this.#cover = node; this.#bytes = node.allocationBytes; return }
    if (this.#linear && this.#cover !== null) {
      if (ancestor(node, this.#cover)) return
      if (ancestor(this.#cover, node)) { this.#cover = node; this.#bytes = node.allocationBytes; return }
    }
    this.#rebuild(stage)
  }
  remove(buffer: ToolOutputBuffer, stage: (bytes: number) => void): void {
    if (!this.#heads.delete(buffer)) throw new Error("missing source head owner")
    if (this.#heads.size === 0) { this.#cover = null; this.#bytes = 0; this.#linear = true; return }
    if (this.#linear) {
      let deepest: ToolOutputNode | null = null
      for (const head of this.#heads) {
        const node = head.allocationNode
        if (node !== null && (deepest === null || node.depth > deepest.depth)) deepest = node
      }
      this.#cover = deepest; this.#bytes = deepest?.allocationBytes ?? 0
    } else this.#rebuild(stage)
  }
  #rebuild(stage: (bytes: number) => void): void {
    // Divergent immutable branches are bounded by each buffer's fixed chunk ceiling.
    const nodes = new Set<ToolOutputNode>()
    let bytes = 0
    for (const head of this.#heads) {
      let node = head.allocationNode
      while (node !== null && !nodes.has(node)) {
        stage(40)
        nodes.add(node)
        bytes += node.allocationBytes - (node.previous?.allocationBytes ?? 0)
        node = node.previous
      }
    }
    this.#linear = false; this.#cover = null; this.#bytes = bytes
  }
}
function ancestor(earlier: ToolOutputNode, later: ToolOutputNode): boolean {
  if (earlier.depth > later.depth) return false
  let node: ToolOutputNode | null = later
  while (node !== null && node.depth > earlier.depth) node = node.previous
  return node === earlier
}
