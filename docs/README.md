# virtx docs

The pages of the virtx documentation, kept next to the code they describe. virtx.sh reads this
folder and serves it at <https://virtx.sh/docs/> with the site's own look; this folder holds what
the docs say, not how they look.

```
content/      the pages, as MDX; the path is the address: guides/using-gpus.mdx is /docs/guides/using-gpus/
sidebar.json  the sidebar: its groups and the order of the pages
preview/      the preview's own code: its content config and the link check
```

## Preview

```sh
npm install
npm run dev      # http://localhost:4321/docs/
npm run build    # builds the preview, then checks every internal link and #anchor
```

The preview is stock Starlight: it shows the pages as they render, not as virtx.sh styles them.
To see them as the site shows them, run virtx.sh with this folder checked out next to it.

## What a page may use

A page is rendered twice, by the preview and by virtx.sh, so it uses only what both have.

- Markdown, and the frontmatter fields Starlight defines (`title`, `description`,
  `tableOfContents`, ...).
- Starlight's own components, imported from `@astrojs/starlight/components` (`Tabs`, `TabItem`,
  `Aside`, ...). Nothing imported from anywhere else: virtx.sh's files are not here.
- Links to other pages as absolute paths with `/docs/`: `[Ports](/docs/guides/ports/)`. They
  resolve the same in both, and the link check verifies them.
- Links to virtx.sh's own pages (`/signin/`, `/signup/`) as plain paths. The preview does not
  have them; add one to `SITE_PAGES` in `preview/linkcheck.mjs` before linking to it.

## Adding a page

Add the file under `content/` and its entry to `sidebar.json`. A `slug` is the page's path under
`content/` without the extension; a `link` is an address under `/docs/` (`"/"` is the overview).
virtx.sh builds its sidebar and the page order of its llms.txt from the same file.
