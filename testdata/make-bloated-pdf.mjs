// Delta.pdf — deliberately bloated: 8 pages of uncompressed, highly redundant
// content streams. Compress should shrink it dramatically (verifiable win).
import { writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const PAGES = 8;

const objects = [];
const pageIds = [];
let nextId = 4;

for (let n = 1; n <= PAGES; n++) {
  const pageId = nextId++;
  const contentId = nextId++;
  pageIds.push(pageId);

  let stream = `0.55 0.14 0.60 rg 0 722 612 70 re f\n`;
  stream += `BT /F1 24 Tf 1 1 1 rg 24 746 Td (Delta.pdf  -  page ${n}  BLOATED) Tj ET\n`;
  stream += `BT /F1 120 Tf 0.15 0.15 0.15 rg 216 356 Td (D${n}) Tj ET\n`;
  // ~50k of redundant invisible-line noise per page, stored uncompressed.
  for (let i = 0; i < 2000; i++) {
    const y = 100 + (i % 600);
    stream += `1 1 1 RG 0.01 w ${50 + (i % 500)} ${y} m ${52 + (i % 500)} ${y} l S\n`;
  }

  objects[pageId] =
    `<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] ` +
    `/Resources << /Font << /F1 3 0 R >> >> /Contents ${contentId} 0 R >>`;
  objects[contentId] = `<< /Length ${stream.length} >>\nstream\n${stream}\nendstream`;
}

objects[1] = `<< /Type /Catalog /Pages 2 0 R >>`;
objects[2] =
  `<< /Type /Pages /Kids [${pageIds.map((i) => `${i} 0 R`).join(" ")}] /Count ${PAGES} >>`;
objects[3] = `<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>`;

let out = "%PDF-1.4\n";
const offsets = [];
for (let i = 1; i < nextId; i++) {
  offsets[i] = Buffer.byteLength(out);
  out += `${i} 0 obj\n${objects[i]}\nendobj\n`;
}
const xrefAt = Buffer.byteLength(out);
out += `xref\n0 ${nextId}\n0000000000 65535 f \n`;
for (let i = 1; i < nextId; i++) out += `${String(offsets[i]).padStart(10, "0")} 00000 n \n`;
out += `trailer\n<< /Size ${nextId} /Root 1 0 R >>\nstartxref\n${xrefAt}\n%%EOF\n`;

writeFileSync(join(HERE, "Delta.pdf"), out, "latin1");
console.log(`Delta.pdf: ${PAGES} pages, ${(out.length / 1e6).toFixed(1)} MB`);
