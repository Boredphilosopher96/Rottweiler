import { defineConfig } from "astro/config"
import { unified } from "@astrojs/markdown-remark"
import sitemap from "@astrojs/sitemap"
import starlight from "@astrojs/starlight"
import linksValidator from "starlight-links-validator"
import productTokens from "./scripts/remark-product-tokens.mjs"

export default defineConfig({
  site: "https://boredphilosopher96.github.io",
  base: "/Rottweiler",
  markdown: {
    processor: unified({ remarkPlugins: [productTokens] }),
  },
  integrations: [
    sitemap(),
    starlight({
      title: "Rottweiler",
      description: "A fast, local coding-agent harness with one secure engine for interactive and automated work.",
      logo: {
        src: "./src/assets/rottweiler-logo.svg",
        alt: "Rottweiler",
      },
      favicon: "/Rottweiler/favicon.svg",
      social: [
        { icon: "github", label: "GitHub", href: "https://github.com/Boredphilosopher96/Rottweiler" },
      ],
      editLink: {
        baseUrl: "https://github.com/Boredphilosopher96/Rottweiler/edit/main/packages/docs-site/",
      },
      customCss: ["./src/styles/custom.css"],
      head: [
        { tag: "meta", attrs: { name: "theme-color", content: "#0b0d10" } },
        { tag: "meta", attrs: { property: "og:type", content: "website" } },
        { tag: "meta", attrs: { property: "og:image", content: "https://boredphilosopher96.github.io/Rottweiler/rottweiler-hero.png" } },
      ],
      plugins: [
        linksValidator({
          errorOnRelativeLinks: false,
          sameSitePolicy: "validate",
        }),
      ],
      sidebar: [
        { label: "Start", items: [
          { label: "Documentation", slug: "docs" },
          { label: "Installation", slug: "docs/installation" },
          { label: "First session", slug: "docs/first-session" },
          { label: "Configure a provider", slug: "docs/providers" },
        ] },
        { label: "Tutorials", items: [{ autogenerate: { directory: "docs/tutorials" } }] },
        { label: "Guides", items: [{ autogenerate: { directory: "docs/guides" } }] },
        { label: "Reference", items: [{ autogenerate: { directory: "docs/reference" } }] },
        { label: "Concepts", items: [{ autogenerate: { directory: "docs/concepts" } }] },
        { label: "Contributing", items: [{ autogenerate: { directory: "contributing" } }] },
      ],
    }),
  ],
})
