// Echo.pdf — a fillable AcroForm fixture: three text fields + two checkboxes.
// NeedAppearances lets the viewer synthesize field appearances.
import { writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));

const fields = [
  { kind: "text", name: "full_name", label: "Full name", x: 180, y: 640, w: 300, h: 28 },
  { kind: "text", name: "email", label: "Email", x: 180, y: 590, w: 300, h: 28 },
  { kind: "text", name: "city", label: "City", x: 180, y: 540, w: 300, h: 28 },
  { kind: "check", name: "agree_terms", label: "I agree to the terms", x: 180, y: 490, w: 20, h: 20 },
  { kind: "check", name: "subscribe", label: "Subscribe to updates", x: 180, y: 450, w: 20, h: 20 },
];

const objects = [];
let nextId = 4;
const pageId = nextId++;
const contentId = nextId++;
const fieldIds = fields.map(() => nextId++);

let stream = `0.16 0.35 0.60 rg 0 722 612 70 re f\n`;
stream += `BT /F1 24 Tf 1 1 1 rg 24 746 Td (Echo.pdf  -  fillable form) Tj ET\n`;
for (const f of fields) {
  stream += `BT /F1 14 Tf 0.2 0.2 0.2 rg ${f.x - 156} ${f.y + 8} Td (${f.label}:) Tj ET\n`;
}

objects[1] =
  `<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [${fieldIds
    .map((i) => `${i} 0 R`)
    .join(" ")}] /NeedAppearances true /DA (/Helv 0 Tf 0 g) ` +
  `/DR << /Font << /Helv 3 0 R >> >> >> >>`;
objects[2] = `<< /Type /Pages /Kids [${pageId} 0 R] /Count 1 >>`;
objects[3] = `<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>`;
objects[pageId] =
  `<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] ` +
  `/Resources << /Font << /F1 3 0 R /Helv 3 0 R >> >> /Contents ${contentId} 0 R ` +
  `/Annots [${fieldIds.map((i) => `${i} 0 R`).join(" ")}] >>`;
objects[contentId] = `<< /Length ${stream.length} >>\nstream\n${stream}\nendstream`;

fields.forEach((f, i) => {
  const rect = `[${f.x} ${f.y} ${f.x + f.w} ${f.y + f.h}]`;
  if (f.kind === "text") {
    objects[fieldIds[i]] =
      `<< /Type /Annot /Subtype /Widget /FT /Tx /T (${f.name}) /Rect ${rect} ` +
      `/F 4 /DA (/Helv 12 Tf 0 g) /MK << /BC [0.3 0.3 0.3] /BG [1 1 1] >> /Ff 0 >>`;
  } else {
    objects[fieldIds[i]] =
      `<< /Type /Annot /Subtype /Widget /FT /Btn /T (${f.name}) /Rect ${rect} ` +
      `/F 4 /V /Off /AS /Off /MK << /BC [0.3 0.3 0.3] /BG [1 1 1] /CA (4) >> >>`;
  }
});

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

writeFileSync(join(HERE, "Echo.pdf"), out, "latin1");
console.log("Echo.pdf: 3 text fields + 2 checkboxes");
