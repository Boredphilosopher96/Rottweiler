# Public documentation site

The Pages branch is also a signed update origin. The release workflow owns
`gh-pages:/updates/**`, including immutable archives and no-redirect channel
metadata. Documentation publication must preserve that subtree byte for byte
and must not race a release publication.

## Decision

Use Astro 7 and Starlight as the site framework. `packages/docs-site` is the
only owner of the public documentation feature: navigation, visual theme,
search, copyable examples, HTML, raw Markdown, the agent index, and the
publication manifest.

`/docs/` is the single product documentation tree. `/raw/product/` contains its
agent-readable projection. Contributor material lives at
`/contributing/` and `/raw/contributing/`.

Explanatory prose and tutorials are authored once in the site content tree.
Facts already owned by executable code or machine contracts remain there:

| Fact | Owner | Documentation projection |
|---|---|---|
| Released targets and archive URLs | signed update metadata | platforms and installation pages |
| Archive membership and supported build shapes | `contracts/release-contract.json` | source-build reference |
| CLI syntax | `rw-cli` Clap command tree | command reference checked against `rw --help` |
| Config fields and typed defaults | `rw-types::Config` | configuration reference |
| Config discovery and precedence | `rw-store::config` | configuration reference |
| Plugin methods, values, limits, schema, and fixture | `rw-plugin-protocol` and its code generator | copied protocol artifacts and reference |
| Client commands and events | `rw-types` protocol code generator | copied schemas and reference |
| Durable session envelope | session schema constant and checked schema | copied schema and reference |

The site build copies owner-generated schemas and fixtures under user-facing
Plugin API and Client API names. It never recreates them. A versioned
`docs-index.json`, `llms.txt`, `llms-full.txt`, and raw
Markdown are generated from the same content collection used to render HTML.
The public search and agent corpus include product and contributor content
from the site collection.

Use the repository-pinned Bun version for dependency management and Node 24 for
Astro execution. TypeDoc is not part of this architecture: TypeDoc 0.28 does
not support the repository's TypeScript 7 compiler. SDK reference pages are
authored against the small public export surface and guarded by SDK tests.

## Publication boundary

Keep branch-based Pages publication. A docs workflow checks out `gh-pages`,
removes only paths named by the previous docs manifest, overlays the new build,
and writes the next manifest. The publisher rejects unsafe manifest entries,
symlinks, `.git`, and `updates`. It records the `updates` Git tree before and
after the transaction and refuses to publish if the tree changes.

The docs publication job and the release workflow's update publication job use
the same job-level concurrency group with cancellation disabled. The release
workflow keeps its existing release-wide concurrency.

## Alternatives considered

- A custom HTML renderer was rejected because it would re-own navigation,
  search, accessibility, copy buttons, Markdown rendering, and responsive UI.
- Docusaurus was rejected because its React runtime and copied version trees add
  complexity without improving the static reference experience.
- TypeDoc was rejected until it supports the repository's compiler version.
- GitHub's Pages artifact deployment was rejected because it replaces the
  published tree instead of preserving the release-owned `/updates` subtree.

## Verification

The site must type-check and build at the real
`/Rottweiler/` base path, static links and agent projections pass, the hostile
overlay test proves `updates/**` is unchanged, repository documentation checks
pass, and publication must use the verified source commit. The live
site and the signed update URLs must both remain readable afterward.
