import { readFileSync } from "node:fs"
import { fileURLToPath } from "node:url"

const specPath = fileURLToPath(new URL("../../../../release/update/stable.spec.json", import.meta.url))
const contractPath = fileURLToPath(new URL("../../../../contracts/release-contract.json", import.meta.url))
const spec = JSON.parse(readFileSync(specPath, "utf8"))
const contract = JSON.parse(readFileSync(contractPath, "utf8"))
const versions = [...new Set(Object.values(spec.targets).map((target) => target.version))]
if (spec.schema_version !== 1 || spec.channel !== "stable" || versions.length !== 1 || contract.schema_version !== 1) {
  throw new Error("stable update spec must own one published product version")
}

const platforms = new Map(contract.platforms.map((platform) => [platform.id, platform]))
export const stableTargets = Object.entries(spec.targets).sort(([left], [right]) => left.localeCompare(right)).map(([id, target]) => {
  const platform = platforms.get(id)
  if (!platform || typeof target.url !== "string") throw new Error(`stable target ${id} must exist in the release contract`)
  return Object.freeze({
    id,
    operatingSystem: platform.distribution.label,
    machine: platform.machine,
    archiveUrl: target.url,
  })
})

export const stableVersion = versions[0]
export const stableTag = `v${stableVersion}`
