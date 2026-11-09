import { readFileSync } from 'node:fs';
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// A preview of the docs: stock Starlight, to read the pages as they render. virtx.sh serves
// them at virtx.sh/docs/ with its own look, from this same folder, so nothing here sets the
// site's look. What both read is content/ (the pages) and sidebar.json (the sidebar's groups and order);
// see README.md for what a page may use.

const sidebar = JSON.parse(readFileSync(new URL('./sidebar.json', import.meta.url), 'utf8'));

// The site serves the docs under /docs/, so the preview does too: a link written /docs/... works
// in both. The site root is outside the preview; `npm run dev` sends it to /docs/.
const rootToDocs = {
  name: 'root-to-docs',
  // Astro puts its base handling, which answers / with a 404, at the front of the stack once the
  // server is configured; this runs after it (post) and goes in front of it.
  enforce: 'post',
  configureServer(server) {
    return () => server.middlewares.stack.unshift({ route: '', handle: redirectRoot });
  }
};
function redirectRoot(req, res, next) {
  if (req.url !== '/') return next();
  res.writeHead(302, { Location: '/docs/' });
  res.end();
}

export default defineConfig({
  site: 'https://virtx.sh',
  trailingSlash: 'always',
  build: { format: 'directory' },
  base: '/docs',
  // the preview's own code (the content config) is kept apart from the pages
  srcDir: './preview',
  integrations: [
    starlight({
      title: 'virtx docs',
      // the site's icon, from the site: the preview has no public/ of its own
      favicon: 'https://virtx.sh/favicon.svg',
      sidebar,
      expressiveCode: { defaultProps: { frame: 'code' } }
    })
  ],
  vite: { plugins: [rootToDocs] }
});
