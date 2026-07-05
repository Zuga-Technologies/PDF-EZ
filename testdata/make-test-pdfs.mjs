// Dependency-free generator for PD-EZ test fixtures.
// Each file: big page labels (A1, A2...), a colored banner, and the filename,
// so merge order and page identity are obvious at a glance.
import { writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));

function makePdf(label, color, pages) {
  const objects = []; // 1-indexed object bodies (without "N 0 obj")
  const pageObjIds = [];
  const contentObjIds = [];

  const catalogId = 1;
  const pagesId = 2;
  const fontId = 3;
  let nextId = 4;

  for (const { w, h, n } of pages) {
    const pageId = nextId++;
    const contentId = nextId++;
    pageObjIds.push(pageId);
    contentObjIds.push(contentId);

    const banner = `${color.join(" ")} rg 0 ${h - 70} ${w} 70 re f`;
    const text =
      `BT /F1 120 Tf 0.15 0.15 0.15 rg ${w / 2 - 90} ${h / 2 - 40} Td (${label}${n}) Tj ET`;
    const header =
      `BT /F1 24 Tf 1 1 1 rg 24 ${h - 46} Td (${label}.pdf  -  page ${n}) Tj ET`;
    const stream = `${banner}\n${header}\n${text}`;

    objects[pageId] =
      `<< /Type /Page /Parent ${pagesId} 0 R /MediaBox [0 0 ${w} ${h}] ` +
      `/Resources << /Font << /F1 ${fontId} 0 R >> >> /Contents ${contentId} 0 R >>`;
    objects[contentId] = `<< /Length ${stream.length} >>\nstream\n${stream}\nendstream`;
  }

  objects[catalogId] = `<< /Type /Catalog /Pages ${pagesId} 0 R >>`;
  objects[pagesId] =
    `<< /Type /Pages /Kids [${pageObjIds.map((i) => `${i} 0 R`).join(" ")}] ` +
    `/Count ${pageObjIds.length} >>`;
  objects[fontId] = `<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>`;

  let out = "%PDF-1.4\n";
  const offsets = [];
  for (let i = 1; i < nextId; i++) {
    offsets[i] = Buffer.byteLength(out);
    out += `${i} 0 obj\n${objects[i]}\nendobj\n`;
  }
  const xrefAt = Buffer.byteLength(out);
  out += `xref\n0 ${nextId}\n0000000000 65535 f \n`;
  for (let i = 1; i < nextId; i++) {
    out += `${String(offsets[i]).padStart(10, "0")} 00000 n \n`;
  }
  out += `trailer\n<< /Size ${nextId} /Root ${catalogId} 0 R >>\nstartxref\n${xrefAt}\n%%EOF\n`;

  writeFileSync(join(HERE, `${label}.pdf`), out, "latin1");
  console.log(`${label}.pdf: ${pages.length} pages`);
}

const P = (n) => ({ w: 612, h: 792, n }); // US Letter portrait
const L = (n) => ({ w: 792, h: 612, n }); // landscape

makePdf("Alpha", [0.78, 0.22, 0.18], [P(1), P(2), P(3)]);
makePdf("Bravo", [0.17, 0.29, 0.80], [P(1), P(2)]);
makePdf("Charlie", [0.11, 0.55, 0.30], [P(1), L(2), P(3), L(4)]);
