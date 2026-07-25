/**
 * Copies the generated API references into the site's static assets so the
 * navbar "API" links work in production:
 *   ../target/doc  (cargo doc --no-deps --document-private-items) -> static/api/rust/
 *   ../ui/docs     (npm run docs in ui/, TypeDoc)                 -> static/api/ui/
 *
 * Missing sources are skipped with a warning — the site still builds, the
 * corresponding API link just 404s until the docs are generated.
 */
import {cpSync, existsSync, rmSync} from 'node:fs';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

const targets = [
  {src: join(root, '..', 'target', 'doc'), dest: join(root, 'static', 'api', 'rust'), name: 'rustdoc'},
  {src: join(root, '..', 'ui', 'docs'), dest: join(root, 'static', 'api', 'ui'), name: 'TypeDoc'},
];

for (const {src, dest, name} of targets) {
  if (!existsSync(src)) {
    console.warn(`[sync-api-docs] ${name} not found at ${src} — skipping (generate it first)`);
    continue;
  }
  rmSync(dest, {recursive: true, force: true});
  cpSync(src, dest, {recursive: true});
  console.log(`[sync-api-docs] ${name}: ${src} -> ${dest}`);
}
