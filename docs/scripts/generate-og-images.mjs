import { mkdir, readFile, readdir } from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import sharp from "sharp";

const DOCS_DIR = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const DEFAULT_OUTPUT_DIR = resolve(DOCS_DIR, ".vitepress/dist/og");

const WIDTH = 1200;
const HEIGHT = 630;

export function ogImagePath(relativePath) {
  return `og/${relativePath.replace(/\.md$/, ".png")}`;
}

function stripMarkdown(value) {
  return value
    .replace(/`([^`]+)`/g, "$1")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/[*_~]/g, "")
    .trim();
}

function unquote(value) {
  if (
    value.length >= 2 &&
    ((value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'")))
  ) {
    return value.slice(1, -1);
  }
  return value;
}

export function titleFromMarkdown(markdown, relativePath) {
  if (relativePath === "index.md") return "A fast Node.js package manager";

  const frontmatter = markdown.match(/^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/)?.[1];
  const frontmatterTitle = frontmatter
    ?.split(/\r?\n/)
    .find((line) => /^title\s*:/.test(line))
    ?.replace(/^title\s*:\s*/, "");
  if (frontmatterTitle) return stripMarkdown(unquote(frontmatterTitle));

  let inFence = false;
  for (const line of markdown.split(/\r?\n/)) {
    if (/^\s*(```|~~~)/.test(line)) {
      inFence = !inFence;
      continue;
    }
    if (!inFence && /^#\s+/.test(line)) {
      return stripMarkdown(line.replace(/^#\s+/, ""));
    }
  }

  return relativePath
    .replace(/(?:^|\/)index\.md$/, "")
    .replace(/\.md$/, "")
    .split("/")
    .filter(Boolean)
    .at(-1)
    ?.replaceAll("-", " ") ?? "Documentation";
}

function escapeXml(value) {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&apos;");
}

function wrapTitle(title, maxUnits) {
  const words = title.split(/\s+/);
  const lines = [];
  let line = "";

  for (const word of words) {
    const candidate = line ? `${line} ${word}` : word;
    if (line && candidate.length > maxUnits) {
      lines.push(line);
      line = word;
    } else {
      line = candidate;
    }
  }
  if (line) lines.push(line);

  return lines;
}

function fitTitle(title) {
  for (const fontSize of [110, 92, 76, 66]) {
    // The title area is 1,048px wide. At these weights a sans glyph averages
    // roughly 0.56em; the small safety margin keeps platform font differences
    // from clipping the right edge.
    const maxUnits = Math.floor(1020 / (fontSize * 0.56));
    const lines = wrapTitle(title, maxUnits);
    if (lines.length <= 3) return { fontSize, lines };
  }

  const fontSize = 60;
  const maxUnits = Math.floor(1020 / (fontSize * 0.56));
  const lines = wrapTitle(title, maxUnits).slice(0, 3);
  lines[2] = `${lines[2].slice(0, Math.max(1, maxUnits - 1)).trimEnd()}…`;
  return { fontSize, lines };
}

export function renderOgSvg(title) {
  const { fontSize, lines } = fitTitle(title);
  const lineHeight = Math.round(fontSize * 1.04);
  const titleY = lines.length === 1 ? 270 : lines.length === 2 ? 230 : 196;
  const tspans = lines
    .map(
      (line, index) =>
        `<tspan x="76" y="${titleY + index * lineHeight}">${escapeXml(line)}</tspan>`,
    )
    .join("");

  return `<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="${WIDTH}" height="${HEIGHT}" viewBox="0 0 ${WIDTH} ${HEIGHT}">
  <defs>
    <radialGradient id="glow" cx="0" cy="0" r="1" gradientTransform="translate(600 -10) scale(520 330)" gradientUnits="userSpaceOnUse">
      <stop stop-color="#5ee0ba" stop-opacity=".28"/>
      <stop offset=".55" stop-color="#e2b046" stop-opacity=".13"/>
      <stop offset="1" stop-color="#f07851" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="sunrise" x1="0" y1="0" x2="1" y2="1">
      <stop stop-color="#5ee0ba"/>
      <stop offset=".55" stop-color="#e2b046"/>
      <stop offset="1" stop-color="#f07851"/>
    </linearGradient>
  </defs>
  <rect width="1200" height="630" fill="#100f0d"/>
  <rect width="1200" height="630" fill="url(#glow)"/>
  <rect x="28" y="28" width="1144" height="574" rx="20" fill="none" stroke="#39332d"/>

  <text x="76" y="104" fill="#f07851" font-family="monospace" font-size="24" font-weight="600" letter-spacing="4">AUBE DOCUMENTATION</text>
  <text fill="#f4f0e8" font-family="Arial, Helvetica, sans-serif" font-size="${fontSize}" font-weight="600" letter-spacing="-2">${tspans}</text>

  <g transform="translate(76 506)">
    <path d="M0 48H72" stroke="#f4f0e8" stroke-width="8" stroke-linecap="round"/>
    <path d="M15 48A21 21 0 0 1 57 48" stroke="url(#sunrise)" stroke-width="8" stroke-linecap="round"/>
    <text x="96" y="58" fill="#f4f0e8" font-family="Georgia, serif" font-size="58">aube</text>
  </g>
  <text x="1124" y="561" text-anchor="end" fill="#aba297" font-family="monospace" font-size="29">aube.sh</text>
</svg>`;
}

async function markdownFiles(directory) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if (entry.name.startsWith(".") || entry.name === "node_modules") continue;
    const path = join(directory, entry.name);
    if (entry.isDirectory()) files.push(...(await markdownFiles(path)));
    else if (entry.isFile() && entry.name.endsWith(".md")) files.push(path);
  }
  return files;
}

export async function generateOgImages(outputDir = DEFAULT_OUTPUT_DIR) {
  const files = await markdownFiles(DOCS_DIR);
  await Promise.all(
    files.map(async (file) => {
      const relativePath = relative(DOCS_DIR, file).split(sep).join("/");
      const markdown = await readFile(file, "utf8");
      const title = titleFromMarkdown(markdown, relativePath);
      const output = resolve(outputDir, ogImagePath(relativePath).slice(3));
      await mkdir(dirname(output), { recursive: true });
      await sharp(Buffer.from(renderOgSvg(title))).png().toFile(output);
    }),
  );
  return files.length;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const outputDir = process.argv[2] ? resolve(process.argv[2]) : DEFAULT_OUTPUT_DIR;
  const count = await generateOgImages(outputDir);
  console.log(`generated ${count} Open Graph images in ${outputDir}`);
}
