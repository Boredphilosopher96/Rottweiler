const FILETYPES: Readonly<Record<string, string>> = {
  bash: "bash",
  c: "c",
  cc: "cpp",
  cpp: "cpp",
  cs: "csharp",
  css: "css",
  diff: "diff",
  go: "go",
  h: "c",
  hpp: "cpp",
  html: "html",
  java: "java",
  js: "javascript",
  jsx: "javascriptreact",
  json: "json",
  lua: "lua",
  md: "markdown",
  patch: "diff",
  php: "php",
  py: "python",
  rb: "ruby",
  rs: "rust",
  sh: "bash",
  toml: "toml",
  ts: "typescript",
  tsx: "typescriptreact",
  yaml: "yaml",
  yml: "yaml",
  zig: "zig",
  zsh: "bash",
}

/** Map file extensions to the canonical names understood by Tree-sitter configs. */
export function filetypeForPath(path: string): string | undefined {
  const name = path.replaceAll("\\", "/").split("/").at(-1)?.toLocaleLowerCase() ?? ""
  if (name === "makefile") return "make"
  const dot = name.lastIndexOf(".")
  if (dot < 0 || dot === name.length - 1) return undefined
  return FILETYPES[name.slice(dot + 1)]
}
