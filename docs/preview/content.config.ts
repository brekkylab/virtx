import { defineCollection } from 'astro:content';
import { glob } from 'astro/loaders';
import { docsSchema } from '@astrojs/starlight/schema';

// The pages are in content/, not Starlight's default src/content/docs/: the folder holds the
// docs and nothing else, and virtx.sh reads the same folder.
export const collections = {
  docs: defineCollection({ loader: glob({ base: './content', pattern: '**/*.{md,mdx}' }), schema: docsSchema() })
};
