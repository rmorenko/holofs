#!/usr/bin/env node
// Validate that every anchor link in docs/**/*.md resolves to an actual
// heading, using GitHub's real slug algorithm (`github-slugger`).
//
// Usage: node scripts/check-doc-anchors.mjs
// Exit 0 if all anchors resolve, 1 otherwise.
//
// Install once: npm install --no-save github-slugger

import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, dirname, resolve, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import GithubSlugger from 'github-slugger';

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DOCS_ROOTS = ['docs', 'docs/ru', 'docs/de', 'docs/fr', 'docs/es'];

function listMarkdown(dir) {
  const out = [];
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isFile() && entry.endsWith('.md')) out.push(full);
  }
  return out;
}

function collectAnchors(file) {
  const slugger = new GithubSlugger();
  const anchors = new Set();
  for (const line of readFileSync(file, 'utf8').split('\n')) {
    const m = line.match(/^#{1,6}\s+(.+?)\s*$/);
    if (m) anchors.add(slugger.slug(m[1]));
  }
  return anchors;
}

const anchorCache = new Map();
function anchorsFor(file) {
  if (!anchorCache.has(file)) {
    try {
      anchorCache.set(file, collectAnchors(file));
    } catch {
      anchorCache.set(file, null);
    }
  }
  return anchorCache.get(file);
}

const broken = [];
let totalRefs = 0;

for (const root of DOCS_ROOTS) {
  const dir = join(REPO_ROOT, root);
  for (const file of listMarkdown(dir)) {
    const own = anchorsFor(file);
    const lines = readFileSync(file, 'utf8').split('\n');
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      const rel = relative(REPO_ROOT, file);

      // Intra-file: ](#anchor)
      for (const m of line.matchAll(/\]\(#([^)]+)\)/g)) {
        totalRefs++;
        if (!own.has(m[1])) {
          broken.push(`${rel}:${i + 1}: intra-file anchor #${m[1]} not found`);
        }
      }

      // Cross-file: ](path.md#anchor) or (./path.md#anchor)
      for (const m of line.matchAll(/\]\((\.\/)?([^)#\s]+\.md)#([^)]+)\)/g)) {
        totalRefs++;
        const target = resolve(dirname(file), m[2]);
        const targetAnchors = anchorsFor(target);
        if (targetAnchors === null) {
          broken.push(`${rel}:${i + 1}: target ${m[2]} does not exist`);
        } else if (!targetAnchors.has(m[3])) {
          broken.push(`${rel}:${i + 1}: cross-file anchor ${m[2]}#${m[3]} not found`);
        }
      }
    }
  }
}

if (broken.length === 0) {
  console.log(`OK: ${totalRefs} anchor references resolve across all docs.`);
  process.exit(0);
}

console.error(`FAIL: ${broken.length} broken anchor references (of ${totalRefs} total):\n`);
for (const b of broken) console.error(`  ${b}`);
process.exit(1);
