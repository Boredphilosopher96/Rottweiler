declare module "*.scm" {
  const embeddedPath: string
  export default embeddedPath
}

declare module "*.wasm" {
  const embeddedPath: string
  export default embeddedPath
}

declare module "*parser.worker.js" {
  const embeddedPath: string
  export default embeddedPath
}

declare module "*tree-sitter.js" {
  const embeddedPath: string
  export default embeddedPath
}
