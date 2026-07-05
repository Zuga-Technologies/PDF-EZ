<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from "vue";
import { confirm, open, save } from "@tauri-apps/plugin-dialog";
import { api, type DocInfo, type FieldInfo, type ObjInfo, type RedactMatch, type SearchHit, type Settings } from "../lib/api";
import Icon from "./Icon.vue";

interface RenderSlot {
  url: string | null;
  renderedWidth: number; // device px the current bitmap was rendered at
  loading: boolean;
}

const doc = ref<DocInfo | null>(null);
const toolsOpen = ref(true);
const railOpen = ref(true);
const pages = reactive<RenderSlot[]>([]);
const thumbs = reactive<RenderSlot[]>([]);
const selected = ref<Set<number>>(new Set());
const dirty = ref(false);
const busy = ref(false);
const error = ref<string | null>(null);
const status = ref<string | null>(null);
const zoom = ref(1.0);
const needsPassword = ref(false);
const password = ref("");
const pendingPath = ref<string | null>(null);
const dragFrom = ref<number | null>(null);
const dragOver = ref<number | null>(null);

/// `autofocus` is unreliable on dynamically inserted elements in Chromium —
/// focus explicitly when the element mounts (and select text for overwrite).
/// Deferred a frame: focusing during the opening click's pointerdown lets the
/// click's default processing steal focus right back, instantly blur-closing
/// the editor.
const vFocus = {
  mounted(el: HTMLInputElement | HTMLTextAreaElement) {
    requestAnimationFrame(() => {
      el.focus();
      if (el instanceof HTMLInputElement) el.select();
    });
  },
};

const ZOOM_STEPS = [0.5, 0.67, 0.8, 1.0, 1.25, 1.5, 2.0, 3.0];
const POINTS_TO_CSS = 96 / 72;
const THUMB_CSS_WIDTH = 132;

// ---- annotation tools ----

type Tool = "select" | "highlight" | "note" | "ink" | "edit" | "redact";
const tool = ref<Tool>("select");
const HIGHLIGHT_COLOR = { r: 255, g: 220, b: 60, a: 128 };
const INK_COLOR = { r: 43, g: 74, b: 203, a: 255 };
const INK_WIDTH_PT = 2;

interface DragState {
  page: number;
  startX: number;
  startY: number;
  x: number;
  y: number;
  points: [number, number][]; // ink trail, CSS px
}
const drag = ref<DragState | null>(null);
const noteDraft = ref<{ page: number; x: number; y: number; text: string } | null>(null);
const undoStack: { page: number; index: number }[] = [];

// ---- redaction ----
// A region marked for removal. Coordinates are display-space PDF points
// (bottom-left origin) — the same space the engine expects.
interface PendingRedaction {
  page: number;
  left: number;
  bottom: number;
  right: number;
  top: number;
}
const pendingRedactions = ref<PendingRedaction[]>([]);
const redactCount = computed(() => pendingRedactions.value.length);
const redactScanOpen = ref(false);
const scanning = ref(false);
const redactKinds = reactive({ SSN: true, Email: true, Card: true, Phone: false });
const redactMatches = ref<(RedactMatch & { chosen: boolean })[]>([]);

function redactCss(r: PendingRedaction) {
  const scale = POINTS_TO_CSS * zoom.value;
  const pageH = doc.value!.pages[r.page].height;
  return {
    left: r.left * scale + "px",
    top: (pageH - r.top) * scale + "px",
    width: Math.max((r.right - r.left) * scale, 2) + "px",
    height: Math.max((r.top - r.bottom) * scale, 2) + "px",
  };
}

function removePendingRedaction(target: PendingRedaction) {
  pendingRedactions.value = pendingRedactions.value.filter((r) => r !== target);
}

async function scanForPatterns() {
  if (!doc.value || busy.value) return;
  const kinds = Object.entries(redactKinds)
    .filter(([, on]) => on)
    .map(([k]) => k);
  if (!kinds.length) return;
  scanning.value = true;
  error.value = null;
  try {
    const found = await api.findPatterns(doc.value.id, kinds);
    redactMatches.value = found.map((m) => ({ ...m, chosen: true }));
  } catch (e) {
    error.value = String(e);
  } finally {
    scanning.value = false;
  }
}

function addChosenMatches() {
  for (const m of redactMatches.value) {
    if (!m.chosen) continue;
    pendingRedactions.value.push({
      page: m.page,
      left: m.left,
      bottom: m.bottom,
      right: m.right,
      top: m.top,
    });
  }
  redactMatches.value = [];
  redactScanOpen.value = false;
  tool.value = "redact";
  status.value = `${pendingRedactions.value.length} region(s) marked — review, then Apply to remove`;
}

// ---- settings (the network contract) ----
const settingsOpen = ref(false);
const settings = ref<Settings>({
  allow_trusted_timestamp: false,
  allow_online_verification: false,
});

async function loadSettings() {
  try {
    settings.value = await api.getSettings();
  } catch {
    /* defaults (fully offline) stand */
  }
}

async function toggleSetting(key: keyof Settings) {
  const next = { ...settings.value, [key]: !settings.value[key] };
  try {
    settings.value = await api.setSettings(next);
  } catch (e) {
    error.value = String(e);
  }
}

// ---- signing (PAdES B-B) ----
const signOpen = ref(false);
const signing = ref(false);
const signPfxPath = ref("");
const signPassword = ref("");
const signReason = ref("");
const signName = ref("");
const signLocation = ref("");
const signError = ref<string | null>(null);

const signPfxName = computed(() => {
  const p = signPfxPath.value;
  const i = Math.max(p.lastIndexOf("/"), p.lastIndexOf("\\"));
  return i >= 0 ? p.slice(i + 1) : p;
});

async function choosePfx() {
  const picked = await open({
    multiple: false,
    filters: [{ name: "Digital ID", extensions: ["pfx", "p12"] }],
  });
  if (typeof picked === "string") signPfxPath.value = picked;
}

async function doSign() {
  if (!doc.value || signing.value) return;
  if (!signPfxPath.value) {
    signError.value = "Choose a .pfx / .p12 digital ID first.";
    return;
  }
  const suggested = doc.value.path.replace(/\.pdf$/i, "") + "-signed.pdf";
  const out = await save({
    defaultPath: suggested,
    filters: [{ name: "PDF", extensions: ["pdf"] }],
  });
  if (!out) return;
  signing.value = true;
  signError.value = null;
  try {
    applyInfo(
      await api.signDocument(
        doc.value.id,
        signPfxPath.value,
        signPassword.value,
        signReason.value,
        signName.value,
        signLocation.value,
        out,
      ),
    );
    dirty.value = false; // the signed file on disk is now the document
    signOpen.value = false;
    signPassword.value = "";
    status.value = "Signed. The document is saved and cryptographically sealed.";
  } catch (e) {
    signError.value = String(e);
  } finally {
    signing.value = false;
  }
}

async function applyRedactions() {
  if (!doc.value || busy.value || !pendingRedactions.value.length) return;
  const n = pendingRedactions.value.length;
  const ok = await confirm(
    `Permanently remove the content under ${n} marked region(s)? This deletes the underlying text and images and cannot be undone.`,
    { title: "Apply redactions", kind: "warning" },
  );
  if (!ok) return;
  busy.value = true;
  error.value = null;
  try {
    const byPage = new Map<number, PendingRedaction[]>();
    for (const r of pendingRedactions.value) {
      const list = byPage.get(r.page) ?? [];
      list.push(r);
      byPage.set(r.page, list);
    }
    for (const [page, rects] of byPage) {
      await api.redactRegions(
        doc.value.id,
        page,
        rects.map((r) => ({ left: r.left, bottom: r.bottom, right: r.right, top: r.top })),
      );
    }
    pendingRedactions.value = [];
    dirty.value = true;
    for (const page of byPage.keys()) await refreshPage(page);
    status.value = `Content removed from ${byPage.size} page(s). Save to keep it.`;
  } catch (e) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

// ---- watermark / header-footer (stamp) ----
const stampOpen = ref(false);
const wmText = ref("CONFIDENTIAL");
const wmSize = ref(64);
const wmOpacity = ref(30);
const wmDiagonal = ref(true);
const hfTop = ref(false);
const hfAlign = ref(1);
const hfTemplate = ref("Page {page} of {pages}");

async function applyWatermark() {
  if (!doc.value) return;
  try {
    applyInfo(await api.addWatermark(doc.value.id, wmText.value, wmSize.value, wmOpacity.value, wmDiagonal.value));
    dirty.value = true;
    stampOpen.value = false;
    status.value = "Watermark applied to every page — Save to keep it";
  } catch (e) {
    error.value = String(e);
  }
}

async function applyHeaderFooter() {
  if (!doc.value) return;
  try {
    applyInfo(await api.addHeaderFooter(doc.value.id, hfTop.value, hfAlign.value, hfTemplate.value));
    dirty.value = true;
    stampOpen.value = false;
    status.value = `${hfTop.value ? "Header" : "Footer"} applied — Save to keep it`;
  } catch (e) {
    error.value = String(e);
  }
}

// ---- edit text & images ----
const objectsByPage = ref<Map<number, ObjInfo[]>>(new Map());
const objEditor = ref<{
  page: number;
  index: number;
  value: string;
  fontPx: number;
  css: { left: string; top: string; width: string; height: string };
} | null>(null);
const selectedObj = ref<{
  page: number;
  index: number;
  kind: string;
  css: { left: string; top: string; width: string; height: string };
} | null>(null);

async function loadObjects(index: number, force = false) {
  if (!doc.value || tool.value !== "edit") return;
  if (!force && objectsByPage.value.has(index)) return;
  try {
    const objects = await api.listPageObjects(doc.value.id, index);
    const next = new Map(objectsByPage.value);
    next.set(index, objects);
    objectsByPage.value = next;
  } catch {
    /* page shows without edit outlines */
  }
}

function objCss(o: ObjInfo) {
  const scale = POINTS_TO_CSS * zoom.value;
  const pageH = doc.value!.pages[o.page].height;
  return {
    left: o.left * scale + "px",
    top: (pageH - o.top) * scale + "px",
    width: Math.max((o.right - o.left) * scale, 4) + "px",
    height: Math.max((o.top - o.bottom) * scale, 4) + "px",
  };
}

function handleEditClick(index: number, px: number, py: number): void {
  const objects = objectsByPage.value.get(index) ?? [];
  const hit = objects
    .filter((o) => px >= o.left && px <= o.right && py >= o.bottom && py <= o.top)
    .sort((a, b) => (a.right - a.left) * (a.top - a.bottom) - (b.right - b.left) * (b.top - b.bottom))[0];
  if (!hit) {
    objEditor.value = null;
    selectedObj.value = null;
    return;
  }
  if (hit.kind === "Text") {
    selectedObj.value = null;
    objEditor.value = {
      page: index,
      index: hit.index,
      value: hit.text,
      fontPx: hit.font_size * POINTS_TO_CSS * zoom.value,
      css: objCss(hit),
    };
  } else {
    objEditor.value = null;
    selectedObj.value = { page: index, index: hit.index, kind: hit.kind, css: objCss(hit) };
  }
}

async function submitObjEditor() {
  const ed = objEditor.value;
  objEditor.value = null;
  if (!ed || !doc.value) return;
  try {
    const objects = await api.setTextObject(doc.value.id, ed.page, ed.index, ed.value);
    const next = new Map(objectsByPage.value);
    next.set(ed.page, objects);
    objectsByPage.value = next;
    dirty.value = true;
    await refreshPage(ed.page);
  } catch (e) {
    error.value = String(e);
  }
}

function onObjEditorBlur(event: FocusEvent) {
  const oid = (event.target as HTMLElement).dataset.oid;
  const ed = objEditor.value;
  if (ed && oid === `${ed.page}:${ed.index}`) void submitObjEditor();
}

async function deleteSelectedObj() {
  const sel = selectedObj.value;
  selectedObj.value = null;
  if (!sel || !doc.value) return;
  try {
    const objects = await api.deletePageObject(doc.value.id, sel.page, sel.index);
    const next = new Map(objectsByPage.value);
    next.set(sel.page, objects);
    objectsByPage.value = next;
    dirty.value = true;
    await refreshPage(sel.page);
  } catch (e) {
    error.value = String(e);
  }
}

watch(tool, (t) => {
  if (t === "edit") {
    for (const index of visiblePages) void loadObjects(index);
  } else {
    objEditor.value = null;
    selectedObj.value = null;
  }
});

// ---- OCR ----
const ocrOpen = ref(false);

async function runOcr() {
  if (!doc.value || busy.value) return;
  ocrOpen.value = false;
  busy.value = true;
  error.value = null;
  status.value = `Recognizing ${doc.value.page_count} ${doc.value.page_count === 1 ? "page" : "pages"}… (about a second per page)`;
  try {
    applyInfo(await api.ocrDocument(doc.value.id, 300));
    dirty.value = true;
    status.value = "Text recognized — try Ctrl+F, then Save to keep it";
  } catch (e) {
    status.value = null;
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

// ---- export & create ----
const exportOpen = ref(false);
const exportFormat = ref<"png" | "jpeg" | "txt">("png");
const exportDpi = ref(150);

async function runExport() {
  if (!doc.value) return;
  error.value = null;
  try {
    if (exportFormat.value === "txt") {
      const target = await save({
        title: "Export text",
        defaultPath: title.value.replace(/\.pdf$/i, ".txt"),
        filters: [{ name: "Text", extensions: ["txt"] }],
      });
      if (!target) return;
      const pages = await api.exportText(doc.value.id, target);
      status.value = `Text from ${pages} pages exported`;
    } else {
      const dir = await open({ title: "Export pages into folder", directory: true });
      if (typeof dir !== "string") return;
      busy.value = true;
      const pages = await api.exportPages(
        doc.value.id,
        dir,
        exportFormat.value === "jpeg",
        exportDpi.value,
      );
      status.value = `${pages} pages exported as ${exportFormat.value.toUpperCase()} (${exportDpi.value} DPI)`;
    }
    exportOpen.value = false;
  } catch (e) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function createFromImages() {
  error.value = null;
  const picked = await open({
    title: "Choose images (one page each)",
    filters: [{ name: "Images", extensions: ["png", "jpg", "jpeg", "bmp", "webp", "tiff"] }],
    multiple: true,
  });
  const paths = Array.isArray(picked) ? picked : typeof picked === "string" ? [picked] : [];
  if (!paths.length) return;
  const out = await save({
    title: "Save new PDF as",
    defaultPath: "images.pdf",
    filters: [{ name: "PDF documents", extensions: ["pdf"] }],
  });
  if (!out) return;
  if (!(await confirmDiscard())) return;
  busy.value = true;
  try {
    await api.imagesToPdf(paths, out);
    await loadDocument(out);
    status.value = `Created ${title.value} from ${paths.length} image${paths.length > 1 ? "s" : ""}`;
  } catch (e) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

// ---- protect (password) ----
const protectOpen = ref(false);
const protectPw = ref("");
const protectPw2 = ref("");
const protectError = ref<string | null>(null);

async function applyProtect() {
  if (!doc.value) return;
  protectError.value = null;
  if (protectPw.value.length < 4) {
    protectError.value = "Password must be at least 4 characters.";
    return;
  }
  if (protectPw.value !== protectPw2.value) {
    protectError.value = "Passwords don't match.";
    return;
  }
  try {
    doc.value = await api.setDocumentPassword(doc.value.id, protectPw.value);
    dirty.value = true;
    protectOpen.value = false;
    protectPw.value = protectPw2.value = "";
    status.value = "Password set — Save to encrypt the file (AES-256)";
  } catch (e) {
    protectError.value = String(e);
  }
}

async function removeProtect() {
  if (!doc.value) return;
  try {
    doc.value = await api.setDocumentPassword(doc.value.id, null);
    dirty.value = true;
    protectOpen.value = false;
    status.value = "Password removed — Save to write the unencrypted file";
  } catch (e) {
    protectError.value = String(e);
  }
}

// ---- form fill ----
const fieldsByPage = ref<Map<number, FieldInfo[]>>(new Map());
const fieldEditor = ref<{
  page: number;
  index: number;
  value: string;
  css: { left: string; top: string; width: string; height: string };
} | null>(null);

async function loadFields(index: number) {
  if (!doc.value || fieldsByPage.value.has(index)) return;
  try {
    const fields = await api.listFormFields(doc.value.id, index);
    if (fields.length) {
      const next = new Map(fieldsByPage.value);
      next.set(index, fields);
      fieldsByPage.value = next;
    } else {
      fieldsByPage.value.set(index, []);
    }
  } catch {
    /* non-fatal; page just shows without field tints */
  }
}

function fieldCss(f: FieldInfo) {
  const scale = POINTS_TO_CSS * zoom.value;
  const pageH = doc.value!.pages[f.page].height;
  return {
    left: f.left * scale + "px",
    top: (pageH - f.top) * scale + "px",
    width: Math.max((f.right - f.left) * scale, 4) + "px",
    height: Math.max((f.top - f.bottom) * scale, 4) + "px",
  };
}

async function applyFieldValue(page: number, index: number, value: string) {
  if (!doc.value) return;
  try {
    const fields = await api.setFormField(doc.value.id, page, index, value);
    const next = new Map(fieldsByPage.value);
    next.set(page, fields);
    fieldsByPage.value = next;
    dirty.value = true;
    await refreshPage(page);
  } catch (e) {
    error.value = String(e);
  }
}

function submitFieldEditor() {
  const ed = fieldEditor.value;
  fieldEditor.value = null;
  if (ed) void applyFieldValue(ed.page, ed.index, ed.value);
}

function onEditorBlur(event: FocusEvent) {
  // Only commit if this input still IS the active editor — Tab/Enter moves
  // replace the editor before the old input blurs.
  const fid = (event.target as HTMLElement).dataset.fid;
  const ed = fieldEditor.value;
  if (ed && fid === `${ed.page}:${ed.index}`) submitFieldEditor();
}

/// Reading-order text fields on a page (top-to-bottom, then left-to-right).
function textFieldsInOrder(page: number): FieldInfo[] {
  return (fieldsByPage.value.get(page) ?? [])
    .filter((f) => f.kind === "Text")
    .sort((a, b) => b.top - a.top || a.left - b.left);
}

/// Commit the current editor and move to the next/previous text field.
function moveEditor(delta: number) {
  const ed = fieldEditor.value;
  if (!ed) return;
  const ordered = textFieldsInOrder(ed.page);
  const pos = ordered.findIndex((f) => f.index === ed.index);
  const next = ordered[(pos + delta + ordered.length) % ordered.length];
  void applyFieldValue(ed.page, ed.index, ed.value);
  fieldEditor.value =
    next && next.index !== ed.index
      ? { page: ed.page, index: next.index, value: next.value, css: fieldCss(next) }
      : null;
}

/// Returns true if the click landed on a form field and was handled.
function handleFieldClick(index: number, px: number, py: number, cssX: number, cssY: number): boolean {
  const fields = fieldsByPage.value.get(index) ?? [];
  const hit = fields.find(
    (f) => px >= f.left && px <= f.right && py >= f.bottom && py <= f.top,
  );
  if (!hit) return false;
  if (hit.kind === "Text") {
    void cssX;
    void cssY;
    fieldEditor.value = {
      page: index,
      index: hit.index,
      value: hit.value,
      css: fieldCss(hit),
    };
  } else if (hit.kind === "Checkbox") {
    void applyFieldValue(index, hit.index, hit.checked ? "false" : "true");
  } else if (hit.kind === "RadioButton") {
    void applyFieldValue(index, hit.index, "true");
  } else {
    return false; // combo/list not editable yet — fall through
  }
  return true;
}

// ---- text search ----
const searchQuery = ref("");
const searchHits = ref<SearchHit[]>([]);
const searchCurrent = ref(0); // current hit number
const searchTotal = computed(() =>
  searchHits.value.length ? Math.max(...searchHits.value.map((h) => h.hit)) + 1 : 0,
);
const searchInput = ref<HTMLInputElement | null>(null);

/// Committed marks kept on screen until the engine re-render lands, so the
/// page never shows a gap between preview and painted annotation.
interface PendingMark {
  key: number;
  page: number;
  rect?: { left: number; top: number; width: number; height: number };
  points?: [number, number][];
}
const pendingMarks = ref<PendingMark[]>([]);
let pendingKey = 0;

/// Markup kinds the user may delete — never Link/Widget (form fields).
const DELETABLE_KINDS = new Set([
  "Highlight", "Text", "Ink", "Square", "Circle", "FreeText",
  "Underline", "StrikeOut", "Squiggly", "Stamp",
]);
const selectedAnnot = ref<{
  page: number;
  index: number;
  kind: string;
  css: { left: number; top: number; width: number; height: number };
} | null>(null);

const title = computed(() => {
  // Filename, always — PDF metadata titles are junk too often ("(anonymous)",
  // "Microsoft Word - final_v2.docx") to put in the toolbar.
  const p = doc.value?.path ?? "";
  return p.split(/[\\/]/).pop() ?? "";
});

function cssWidth(index: number): number {
  const dims = doc.value!.pages[index];
  return Math.round(dims.width * POINTS_TO_CSS * zoom.value);
}
function cssHeight(index: number): number {
  const dims = doc.value!.pages[index];
  return Math.round(dims.height * POINTS_TO_CSS * zoom.value);
}
function thumbHeight(index: number): number {
  const dims = doc.value!.pages[index];
  return Math.round((THUMB_CSS_WIDTH * dims.height) / dims.width);
}
function dpr(): number {
  return window.devicePixelRatio || 1;
}

// ---- lazy rendering (shared by main pages and thumbnails) ----

const scroller = ref<HTMLElement | null>(null);
const rail = ref<HTMLElement | null>(null);
const visiblePages = new Set<number>();
const visibleThumbs = new Set<number>();
let pageObserver: IntersectionObserver | null = null;
let thumbObserver: IntersectionObserver | null = null;

function observePage(el: Element | null, index: number) {
  if (!el || !pageObserver) return;
  (el as HTMLElement).dataset.page = String(index);
  pageObserver.observe(el);
}
function observeThumb(el: Element | null, index: number) {
  if (!el || !thumbObserver) return;
  (el as HTMLElement).dataset.page = String(index);
  thumbObserver.observe(el);
}

function makeObserver(
  root: HTMLElement | null,
  visible: Set<number>,
  render: (index: number) => void,
  onExit?: (index: number) => void,
): IntersectionObserver {
  return new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        const index = Number((entry.target as HTMLElement).dataset.page);
        if (entry.isIntersecting) {
          visible.add(index);
          render(index);
        } else {
          visible.delete(index);
          onExit?.(index);
        }
      }
    },
    { root, rootMargin: "600px 0px" },
  );
}

async function renderSlot(slots: RenderSlot[], index: number, want: number, jpeg: boolean) {
  if (!doc.value) return;
  const slot = slots[index];
  if (!slot || slot.loading || (slot.url && slot.renderedWidth === want)) return;
  slot.loading = true;
  try {
    const rendered = await api.renderPage(doc.value.id, index, want, jpeg);
    // Pre-decode before swapping src: assigning a big data-URL directly
    // blanks the <img> for the decode duration (the "blink").
    const pre = new Image();
    pre.src = rendered.data_url;
    await pre.decode().catch(() => {});
    slot.url = rendered.data_url;
    slot.renderedWidth = want;
  } catch (e) {
    error.value = String(e);
  } finally {
    slot.loading = false;
  }
}

function renderPageIfStale(index: number) {
  void loadFields(index);
  void loadObjects(index);
  const want = Math.round(cssWidth(index) * dpr());
  const slot = pages[index];
  if (slot && (!slot.url || slot.renderedWidth !== want)) {
    void renderSlot(pages, index, want, true).then(() => {
      // Zoom may have moved again while this render was in flight.
      const now = Math.round(cssWidth(index) * dpr());
      if (visiblePages.has(index) && pages[index] && pages[index].renderedWidth !== now) {
        renderPageIfStale(index);
      }
    });
  }
}
function renderThumbIfStale(index: number) {
  void renderSlot(thumbs, index, Math.round(THUMB_CSS_WIDTH * dpr()), false);
}
// Full-page bitmaps are big; drop them once a page scrolls far off-screen so
// a long document can't accumulate hundreds of MB inside the webview.
function evictPage(index: number) {
  const slot = pages[index];
  if (slot && !slot.loading) {
    slot.url = null;
    slot.renderedWidth = 0;
  }
}

watch(zoom, () => {
  for (const index of visiblePages) renderPageIfStale(index);
});

// ---- document lifecycle ----

function applyInfo(info: DocInfo) {
  doc.value = info;
  pages.length = 0;
  thumbs.length = 0;
  for (let i = 0; i < info.page_count; i++) {
    pages.push({ url: null, renderedWidth: 0, loading: false });
    thumbs.push({ url: null, renderedWidth: 0, loading: false });
  }
  selected.value = new Set();
  selectedAnnot.value = null;
  fieldEditor.value = null;
  fieldsByPage.value = new Map();
  objEditor.value = null;
  selectedObj.value = null;
  objectsByPage.value = new Map();
  undoStack.length = 0; // annotation indices are stale after any page op
  // Already-visible elements get no new intersection event; refresh them.
  for (const index of visiblePages) renderPageIfStale(index);
  for (const index of visibleThumbs) renderThumbIfStale(index);
}

async function confirmDiscard(): Promise<boolean> {
  if (!dirty.value) return true;
  return confirm("You have unsaved changes. Discard them?", {
    title: "PD-EZ",
    kind: "warning",
  });
}

async function openFile() {
  if (!(await confirmDiscard())) return;
  const path = await open({
    title: "Open PDF",
    filters: [{ name: "PDF documents", extensions: ["pdf"] }],
    multiple: false,
  });
  if (typeof path !== "string") return;
  await loadDocument(path);
}

async function loadDocument(path: string, pw?: string) {
  error.value = null;
  status.value = null;
  needsPassword.value = false;
  try {
    if (doc.value) {
      const old = doc.value.id;
      doc.value = null;
      void api.closeDocument(old);
    }
    const info = await api.openDocument(path, pw);
    dirty.value = false;
    searchQuery.value = "";
    searchHits.value = [];
    applyInfo(info);
  } catch (e) {
    if (String(e).includes("PASSWORD_REQUIRED")) {
      pendingPath.value = path;
      needsPassword.value = true;
      if (pw) error.value = "Wrong password — try again.";
    } else {
      error.value = String(e);
    }
  }
}

function submitPassword() {
  if (pendingPath.value) void loadDocument(pendingPath.value, password.value);
}

// ---- organize operations ----

const selectionList = computed(() => [...selected.value].sort((a, b) => a - b));

function toggleSelect(index: number, event: MouseEvent) {
  const next = new Set(selected.value);
  if (event.ctrlKey || event.metaKey) {
    next.has(index) ? next.delete(index) : next.add(index);
  } else {
    next.clear();
    next.add(index);
  }
  selected.value = next;
}

async function mutate(op: () => Promise<DocInfo>) {
  if (!doc.value || busy.value) return;
  busy.value = true;
  error.value = null;
  status.value = null;
  try {
    const info = await op();
    applyInfo(info);
    dirty.value = true;
  } catch (e) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

function rotateSelection(quarterTurns: number) {
  const indices = selectionList.value;
  if (!indices.length) return;
  const keep = new Set(indices);
  void mutate(() => api.rotatePages(doc.value!.id, indices, quarterTurns)).then(() => {
    selected.value = keep; // rotation doesn't change indices
  });
}

async function deleteSelection() {
  const indices = selectionList.value;
  if (!indices.length || !doc.value) return;
  const noun = indices.length === 1 ? `page ${indices[0] + 1}` : `${indices.length} pages`;
  if (!(await confirm(`Delete ${noun}?`, { title: "PD-EZ", kind: "warning" }))) return;
  void mutate(() => api.deletePages(doc.value!.id, indices));
}

function onDrop(dest: number) {
  const from = dragFrom.value;
  dragFrom.value = null;
  dragOver.value = null;
  if (from === null || !doc.value || from === dest) return;
  const order = [...Array(doc.value.page_count).keys()];
  order.splice(from, 1);
  order.splice(dest, 0, from);
  void mutate(() => api.reorderPages(doc.value!.id, order));
}

// ---- annotation interactions ----

function cssToPdf(index: number, cssX: number, cssY: number): [number, number] {
  const scale = POINTS_TO_CSS * zoom.value;
  const dims = doc.value!.pages[index];
  return [cssX / scale, dims.height - cssY / scale];
}

function pagePoint(event: PointerEvent): [number, number] {
  const el = (event.currentTarget as HTMLElement).getBoundingClientRect();
  return [
    Math.min(Math.max(event.clientX - el.left, 0), el.width),
    Math.min(Math.max(event.clientY - el.top, 0), el.height),
  ];
}

function onPagePointerDown(event: PointerEvent, index: number) {
  if (!doc.value || busy.value) return;
  if (tool.value === "select") {
    // Keep the page from taking focus — it would blur an opening field editor.
    event.preventDefault();
    void selectAnnotAt(event, index);
    return;
  }
  if (tool.value === "edit") {
    event.preventDefault();
    const [cssX, cssY] = pagePoint(event);
    const [px, py] = cssToPdf(index, cssX, cssY);
    handleEditClick(index, px, py);
    return;
  }
  event.preventDefault();
  const [x, y] = pagePoint(event);
  if (tool.value === "note") {
    noteDraft.value = { page: index, x, y, text: "" };
    return;
  }
  (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  drag.value = { page: index, startX: x, startY: y, x, y, points: [[x, y]] };
}

function onPagePointerMove(event: PointerEvent, index: number) {
  if (!drag.value || drag.value.page !== index) return;
  const [x, y] = pagePoint(event);
  drag.value.x = x;
  drag.value.y = y;
  if (tool.value === "ink") drag.value.points.push([x, y]);
}

async function onPagePointerUp(index: number) {
  const d = drag.value;
  drag.value = null;
  if (!d || d.page !== index || !doc.value) return;

  if (tool.value === "highlight") {
    const [x0, y0] = cssToPdf(index, Math.min(d.startX, d.x), Math.max(d.startY, d.y));
    const [x1, y1] = cssToPdf(index, Math.max(d.startX, d.x), Math.min(d.startY, d.y));
    if (Math.abs(x1 - x0) < 2 || Math.abs(y1 - y0) < 2) return; // accidental click
    const mark: PendingMark = {
      key: pendingKey++,
      page: index,
      rect: {
        left: Math.min(d.startX, d.x),
        top: Math.min(d.startY, d.y),
        width: Math.abs(d.x - d.startX),
        height: Math.abs(d.y - d.startY),
      },
    };
    await addAnnotation(index, mark, (id) =>
      api.addHighlight(id, index, { left: x0, bottom: y0, right: x1, top: y1 }, HIGHLIGHT_COLOR),
    );
  } else if (tool.value === "ink") {
    if (d.points.length < 2) return;
    const stroke = d.points.map(([x, y]) => cssToPdf(index, x, y));
    const mark: PendingMark = { key: pendingKey++, page: index, points: d.points };
    await addAnnotation(index, mark, (id) =>
      api.addInk(id, index, [stroke], INK_COLOR, INK_WIDTH_PT),
    );
  } else if (tool.value === "redact") {
    const [x0, y0] = cssToPdf(index, Math.min(d.startX, d.x), Math.max(d.startY, d.y));
    const [x1, y1] = cssToPdf(index, Math.max(d.startX, d.x), Math.min(d.startY, d.y));
    if (Math.abs(x1 - x0) < 2 || Math.abs(y1 - y0) < 2) return; // accidental click
    pendingRedactions.value.push({ page: index, left: x0, bottom: y0, right: x1, top: y1 });
  }
}

async function selectAnnotAt(event: PointerEvent, index: number) {
  if (!doc.value) return;
  const [cssX, cssY] = pagePoint(event);
  const [px, py] = cssToPdf(index, cssX, cssY);
  // Form fields take priority over annotation selection.
  if (handleFieldClick(index, px, py, cssX, cssY)) {
    selectedAnnot.value = null;
    return;
  }
  try {
    const annots = await api.listAnnotations(doc.value.id, index);
    const scale = POINTS_TO_CSS * zoom.value;
    const pageH = doc.value.pages[index].height;
    const PAD = 3; // points — makes thin ink strokes clickable
    // Smallest containing annotation wins, so a note inside a highlight is
    // still selectable.
    const hit = annots
      .filter((a) => DELETABLE_KINDS.has(a.kind))
      .filter(
        (a) =>
          px >= a.left - PAD && px <= a.right + PAD && py >= a.bottom - PAD && py <= a.top + PAD,
      )
      .sort((a, b) => (a.right - a.left) * (a.top - a.bottom) - (b.right - b.left) * (b.top - b.bottom))[0];
    selectedAnnot.value = hit
      ? {
          page: index,
          index: hit.index,
          kind: hit.kind,
          css: {
            left: hit.left * scale,
            top: (pageH - hit.top) * scale,
            width: (hit.right - hit.left) * scale,
            height: (hit.top - hit.bottom) * scale,
          },
        }
      : null;
  } catch (e) {
    error.value = String(e);
  }
}

async function deleteSelectedAnnot() {
  const sel = selectedAnnot.value;
  selectedAnnot.value = null;
  if (!sel || !doc.value) return;
  try {
    await api.deleteAnnotation(doc.value.id, sel.page, sel.index);
    // Undo entries at higher indices on this page shift down by one.
    for (let i = undoStack.length - 1; i >= 0; i--) {
      const u = undoStack[i];
      if (u.page !== sel.page) continue;
      if (u.index === sel.index) undoStack.splice(i, 1);
      else if (u.index > sel.index) u.index--;
    }
    dirty.value = true;
    await refreshPage(sel.page);
  } catch (e) {
    error.value = String(e);
  }
}

async function submitNote() {
  const draft = noteDraft.value;
  noteDraft.value = null;
  if (!draft || !draft.text.trim() || !doc.value) return;
  const [x, y] = cssToPdf(draft.page, draft.x, draft.y);
  await addAnnotation(draft.page, null, (id) =>
    api.addNote(id, draft.page, x, y, draft.text.trim()),
  );
}

async function addAnnotation(
  index: number,
  mark: PendingMark | null,
  op: (docId: number) => Promise<import("../lib/api").AnnotInfo[]>,
) {
  if (!doc.value) return;
  if (mark) pendingMarks.value.push(mark);
  try {
    const annots = await op(doc.value.id);
    if (annots.length) undoStack.push({ page: index, index: annots.length - 1 });
    dirty.value = true;
    await refreshPage(index);
  } catch (e) {
    error.value = String(e);
  } finally {
    if (mark) pendingMarks.value = pendingMarks.value.filter((m) => m.key !== mark.key);
  }
}

async function undoAnnotation() {
  const last = undoStack.pop();
  if (!last || !doc.value) return;
  try {
    await api.deleteAnnotation(doc.value.id, last.page, last.index);
    dirty.value = true;
    await refreshPage(last.page);
  } catch (e) {
    error.value = String(e);
  }
}

/// Re-render a page after a content change WITHOUT dropping the current
/// bitmap — the old image stays up until the new one swaps in (no blink).
async function refreshPage(index: number) {
  const thumb = thumbs[index];
  if (thumb) thumb.renderedWidth = 0;
  if (visibleThumbs.has(index)) renderThumbIfStale(index);

  const page = pages[index];
  if (page) page.renderedWidth = 0;
  if (visiblePages.has(index)) {
    await renderSlot(pages, index, Math.round(cssWidth(index) * dpr()), true);
  }
}

// ---- save ----

async function runSearch() {
  if (!doc.value) return;
  const q = searchQuery.value.trim();
  if (!q) {
    searchHits.value = [];
    return;
  }
  try {
    searchHits.value = await api.searchDocument(doc.value.id, q, false);
    searchCurrent.value = 0;
    if (searchHits.value.length) scrollToHit(0);
  } catch (e) {
    error.value = String(e);
  }
}

function stepSearch(delta: number) {
  if (!searchTotal.value) return;
  searchCurrent.value = (searchCurrent.value + delta + searchTotal.value) % searchTotal.value;
  scrollToHit(searchCurrent.value);
}

function scrollToHit(hit: number) {
  const first = searchHits.value.find((h) => h.hit === hit);
  if (!first || !doc.value || !scroller.value) return;
  const pageEl = scroller.value.querySelector<HTMLElement>(`[data-page="${first.page}"]`);
  if (!pageEl) return;
  const pageH = doc.value.pages[first.page].height;
  const ratio = (pageH - first.top) / pageH;
  scroller.value.scrollTo({
    top: pageEl.offsetTop + pageEl.offsetHeight * ratio - scroller.value.clientHeight / 3,
    behavior: "smooth",
  });
}

function hitCss(h: SearchHit) {
  const scale = POINTS_TO_CSS * zoom.value;
  const pageH = doc.value!.pages[h.page].height;
  return {
    left: h.left * scale + "px",
    top: (pageH - h.top) * scale + "px",
    width: Math.max((h.right - h.left) * scale, 2) + "px",
    height: Math.max((h.top - h.bottom) * scale, 2) + "px",
  };
}

function fmtBytes(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)} MB`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)} KB`;
  return `${n} B`;
}

async function compressDoc() {
  if (!doc.value || busy.value) return;
  busy.value = true;
  error.value = null;
  status.value = "Compressing…";
  try {
    const result = await api.compressDocument(doc.value.id);
    applyInfo(result.info);
    dirty.value = true;
    const saved = result.bytes_before - result.bytes_after;
    const pct = result.bytes_before ? Math.round((saved / result.bytes_before) * 100) : 0;
    status.value =
      saved > 0
        ? `${fmtBytes(result.bytes_before)} → ${fmtBytes(result.bytes_after)} (−${pct}%) — Save to keep it`
        : `Already tight: ${fmtBytes(result.bytes_before)} → ${fmtBytes(result.bytes_after)}`;
  } catch (e) {
    status.value = null;
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function mergeFiles() {
  if (!doc.value || busy.value) return;
  const picked = await open({
    title: "Merge PDFs into this document",
    filters: [{ name: "PDF documents", extensions: ["pdf"] }],
    multiple: true,
  });
  const paths = Array.isArray(picked) ? picked : typeof picked === "string" ? [picked] : [];
  if (!paths.length) return;
  await mutate(() => api.mergeAppend(doc.value!.id, paths));
}

async function saveNow() {
  if (!doc.value || !dirty.value || busy.value) return;
  busy.value = true;
  try {
    doc.value = await api.saveDocument(doc.value.id);
    dirty.value = false;
  } catch (e) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function saveAs() {
  if (!doc.value || busy.value) return;
  const target = await save({
    title: "Save PDF as",
    defaultPath: title.value,
    filters: [{ name: "PDF documents", extensions: ["pdf"] }],
  });
  if (!target) return;
  busy.value = true;
  try {
    doc.value = await api.saveDocument(doc.value.id, target);
    dirty.value = false;
  } catch (e) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

// ---- zoom + keyboard ----

function zoomIn() {
  const next = ZOOM_STEPS.find((z) => z > zoom.value + 0.001);
  if (next) zoom.value = next;
}
function zoomOut() {
  const next = [...ZOOM_STEPS].reverse().find((z) => z < zoom.value - 0.001);
  if (next) zoom.value = next;
}

function onKeydown(e: KeyboardEvent) {
  if (noteDraft.value || fieldEditor.value || objEditor.value) return; // typing in an overlay
  if (e.key === "Delete" && selectedObj.value) {
    void deleteSelectedObj();
    return;
  }
  if (e.ctrlKey && e.key.toLowerCase() === "f") {
    e.preventDefault();
    searchInput.value?.focus();
    return;
  }
  if (document.activeElement === searchInput.value) return; // typing a query
  if (e.ctrlKey && e.key.toLowerCase() === "s") {
    e.preventDefault();
    e.shiftKey ? void saveAs() : void saveNow();
  } else if (e.ctrlKey && e.key.toLowerCase() === "z") {
    e.preventDefault();
    void undoAnnotation();
  } else if (e.key === "Escape") {
    tool.value = "select";
    drag.value = null;
    selectedAnnot.value = null;
  } else if (e.key === "Delete" && selectedAnnot.value) {
    void deleteSelectedAnnot();
  } else if (e.key === "Delete" && selected.value.size) {
    void deleteSelection();
  }
}

onMounted(() => {
  pageObserver = makeObserver(scroller.value, visiblePages, renderPageIfStale, evictPage);
  thumbObserver = makeObserver(rail.value, visibleThumbs, renderThumbIfStale);
  window.addEventListener("keydown", onKeydown);
  void loadSettings();
});

onBeforeUnmount(() => {
  pageObserver?.disconnect();
  thumbObserver?.disconnect();
  window.removeEventListener("keydown", onKeydown);
  if (doc.value) void api.closeDocument(doc.value.id);
});
</script>

<template>
  <div class="viewer">
    <header class="toolbar">
      <button class="tools-toggle" :class="{ active: toolsOpen }" title="Show or hide the tools panel" @click="toolsOpen = !toolsOpen">
        Tools
      </button>
      <button class="iconbtn primary" title="Open PDF (file stays on this machine)" aria-label="Open PDF" @click="openFile">
        <Icon name="folder-open" />
      </button>
      <template v-if="doc">
        <button class="iconbtn" :disabled="!dirty || busy" title="Save (Ctrl+S)" aria-label="Save" @click="saveNow">
          <Icon name="save" />
        </button>
        <button class="iconbtn" :disabled="busy" title="Save as… (Ctrl+Shift+S)" aria-label="Save as" @click="saveAs">
          <Icon name="save-all" />
        </button>
      </template>
      <div class="spacer"></div>
      <template v-if="doc">
        <input
          ref="searchInput"
          v-model="searchQuery"
          class="search"
          type="search"
          placeholder="Find… (Ctrl+F)"
          @keydown.enter.prevent="searchHits.length && searchQuery.trim() ? stepSearch(1) : runSearch()"
          @keydown.escape="((searchQuery = ''), (searchHits = []))"
          @input="searchHits = []"
        />
        <template v-if="searchTotal">
          <span class="meta">{{ searchCurrent + 1 }}/{{ searchTotal }}</span>
          <button class="iconbtn" aria-label="Previous match" title="Previous match" @click="stepSearch(-1)"><Icon name="chevron-left" /></button>
          <button class="iconbtn" aria-label="Next match" title="Next match" @click="stepSearch(1)"><Icon name="chevron-right" /></button>
        </template>
        <span class="divider"></span>
        <span class="doctitle" :title="doc.path">{{ title }}<i v-if="dirty"> •</i></span>
        <span class="meta">{{ doc.page_count }} {{ doc.page_count === 1 ? "page" : "pages" }}</span>
        <button class="iconbtn" aria-label="Zoom out" title="Zoom out" @click="zoomOut"><Icon name="minus" /></button>
        <span class="zoom">{{ Math.round(zoom * 100) }}%</span>
        <button class="iconbtn" aria-label="Zoom in" title="Zoom in" @click="zoomIn"><Icon name="plus" /></button>
      </template>
      <button class="iconbtn" title="Settings — network &amp; privacy" aria-label="Settings" @click="settingsOpen = true">
        <Icon name="settings" />
      </button>
    </header>

    <div v-if="settingsOpen" class="protect-modal" @pointerdown.self="settingsOpen = false">
      <div class="protect-card scan-card">
        <h2>Network &amp; privacy</h2>
        <p class="protect-hint">
          PD-EZ runs entirely on this machine. Nothing you open, edit, or save
          is ever uploaded. The only features that can touch the network are the
          two below — both off unless you turn them on here.
        </p>
        <div class="net-toggle">
          <label class="switch">
            <input
              type="checkbox"
              :checked="settings.allow_trusted_timestamp"
              @change="toggleSetting('allow_trusted_timestamp')"
            />
            <span class="switch-track"></span>
          </label>
          <div class="net-copy">
            <strong>Trusted timestamp</strong>
            <span>When signing, make one call to a timestamp authority so the signature proves <em>when</em> it was made (RFC&nbsp;3161). Off = self-asserted time only.</span>
          </div>
        </div>
        <div class="net-toggle">
          <label class="switch">
            <input
              type="checkbox"
              :checked="settings.allow_online_verification"
              @change="toggleSetting('allow_online_verification')"
            />
            <span class="switch-track"></span>
          </label>
          <div class="net-copy">
            <strong>Online signature verification</strong>
            <span>When checking a signature, contact the issuer to see if the certificate was revoked (OCSP&nbsp;/&nbsp;CRL). Off = integrity, cryptography, and chain math are still verified — just not revocation.</span>
          </div>
        </div>
        <p class="net-foot">
          Every online action is a button you press, labeled with exactly what it
          contacts. No telemetry, no accounts, no background calls — ever.
        </p>
        <div class="protect-actions">
          <span class="spacer"></span>
          <button class="primary" @click="settingsOpen = false">Done</button>
        </div>
      </div>
    </div>

    <div v-if="stampOpen && doc" class="protect-modal" @pointerdown.self="stampOpen = false">
      <div class="protect-card stamp-card">
        <h2>Watermark</h2>
        <div class="stamp-row">
          <input v-model="wmText" v-focus type="text" placeholder="Watermark text" />
        </div>
        <div class="stamp-row">
          <label>Size <input v-model.number="wmSize" type="number" min="8" max="300" /></label>
          <label>Opacity <input v-model.number="wmOpacity" type="range" min="5" max="100" /> {{ wmOpacity }}%</label>
          <label><input v-model="wmDiagonal" type="checkbox" /> Diagonal</label>
        </div>
        <div class="protect-actions">
          <span class="spacer"></span>
          <button class="primary" :disabled="!wmText.trim()" @click="applyWatermark">Apply watermark</button>
        </div>
        <hr />
        <h2>Header / Footer</h2>
        <div class="stamp-row">
          <select v-model="hfTop">
            <option :value="false">Footer (bottom)</option>
            <option :value="true">Header (top)</option>
          </select>
          <select v-model.number="hfAlign">
            <option :value="0">Left</option>
            <option :value="1">Center</option>
            <option :value="2">Right</option>
          </select>
        </div>
        <div class="stamp-row">
          <input v-model="hfTemplate" type="text" placeholder="Page {page} of {pages}" />
        </div>
        <p class="protect-hint">{page} and {pages} are replaced per page. Applies to all pages; reopen without saving to undo.</p>
        <div class="protect-actions">
          <button type="button" @click="stampOpen = false">Close</button>
          <span class="spacer"></span>
          <button class="primary" :disabled="!hfTemplate.trim()" @click="applyHeaderFooter">
            Apply {{ hfTop ? "header" : "footer" }}
          </button>
        </div>
      </div>
    </div>

    <div v-if="protectOpen && doc" class="protect-modal" @pointerdown.self="protectOpen = false">
      <form class="protect-card" @submit.prevent="applyProtect">
        <h2>{{ doc.has_password ? "Change or remove password" : "Password-protect this PDF" }}</h2>
        <p class="protect-hint">
          The file is encrypted with AES-256 when you save. Anyone opening it will need this
          password — there is no recovery if it's lost.
        </p>
        <input v-model="protectPw" v-focus type="password" placeholder="Password" />
        <input v-model="protectPw2" type="password" placeholder="Repeat password" />
        <p v-if="protectError" class="error">{{ protectError }}</p>
        <div class="protect-actions">
          <button v-if="doc.has_password" type="button" class="danger" @click="removeProtect">
            Remove password
          </button>
          <span class="spacer"></span>
          <button type="button" @click="protectOpen = false">Cancel</button>
          <button class="primary" type="submit">Set password</button>
        </div>
      </form>
    </div>

    <div v-if="exportOpen && doc" class="protect-modal" @pointerdown.self="exportOpen = false">
      <div class="protect-card">
        <h2>Export {{ title }}</h2>
        <div class="stamp-row">
          <select v-model="exportFormat">
            <option value="png">Pages → PNG images</option>
            <option value="jpeg">Pages → JPEG images</option>
            <option value="txt">Document → plain text</option>
          </select>
          <select v-if="exportFormat !== 'txt'" v-model.number="exportDpi">
            <option :value="96">96 DPI (screen)</option>
            <option :value="150">150 DPI (sharp)</option>
            <option :value="300">300 DPI (print)</option>
          </select>
        </div>
        <p class="protect-hint">
          {{ exportFormat === "txt"
            ? "All page text is written to one .txt file."
            : "One image per page, named after the document, into a folder you pick." }}
        </p>
        <div class="protect-actions">
          <button type="button" @click="exportOpen = false">Cancel</button>
          <span class="spacer"></span>
          <button class="primary" :disabled="busy" @click="runExport">Export…</button>
        </div>
      </div>
    </div>

    <div v-if="ocrOpen && doc" class="protect-modal" @pointerdown.self="ocrOpen = false">
      <div class="protect-card">
        <h2>Recognize text (OCR)</h2>
        <p class="protect-hint">
          Makes scanned documents searchable: every page is re-rendered as a 300&nbsp;DPI image
          with an invisible text layer (English). Highlights and form values are baked into the
          image. Best for scans — on born-digital PDFs you'd lose editable text, so only run it
          when search finds nothing.
        </p>
        <div class="protect-actions">
          <button type="button" @click="ocrOpen = false">Cancel</button>
          <span class="spacer"></span>
          <button class="primary" :disabled="busy" @click="runOcr">
            Recognize {{ doc.page_count }} {{ doc.page_count === 1 ? "page" : "pages" }}
          </button>
        </div>
      </div>
    </div>

    <div v-if="redactScanOpen && doc" class="protect-modal" @pointerdown.self="redactScanOpen = false">
      <div class="protect-card scan-card">
        <h2>Find sensitive data</h2>
        <p class="protect-hint">
          Scans every page for these patterns and marks each match for redaction. Nothing is
          removed until you Apply — review the list first.
        </p>
        <div class="scan-kinds">
          <label><input type="checkbox" v-model="redactKinds.SSN" /> Social Security numbers</label>
          <label><input type="checkbox" v-model="redactKinds.Email" /> Email addresses</label>
          <label><input type="checkbox" v-model="redactKinds.Card" /> Credit-card numbers</label>
          <label><input type="checkbox" v-model="redactKinds.Phone" /> Phone numbers</label>
        </div>
        <button class="primary scan-run" :disabled="scanning" @click="scanForPatterns">
          {{ scanning ? "Scanning…" : "Scan document" }}
        </button>

        <div v-if="redactMatches.length" class="scan-results">
          <p class="scan-count">{{ redactMatches.length }} match(es) found</p>
          <ul>
            <li v-for="(m, mi) in redactMatches" :key="'m' + mi">
              <label>
                <input type="checkbox" v-model="m.chosen" />
                <span class="scan-kind">{{ m.kind }}</span>
                <span class="scan-sample">{{ m.sample }}</span>
                <span class="scan-page">p.{{ m.page + 1 }}</span>
              </label>
            </li>
          </ul>
        </div>
        <p v-else-if="!scanning && redactMatches" class="scan-empty"></p>

        <div class="protect-actions">
          <button type="button" @click="((redactScanOpen = false), (redactMatches = []))">Cancel</button>
          <span class="spacer"></span>
          <button
            class="primary"
            :disabled="!redactMatches.some((m) => m.chosen)"
            @click="addChosenMatches"
          >
            Mark {{ redactMatches.filter((m) => m.chosen).length }} for redaction
          </button>
        </div>
      </div>
    </div>

    <div v-if="signOpen && doc" class="protect-modal" @pointerdown.self="signOpen = false">
      <div class="protect-card scan-card">
        <h2>Sign this PDF</h2>
        <p class="protect-hint">
          Adds a cryptographic PAdES signature from your digital ID (a
          <code>.pfx</code> / <code>.p12</code> file). Signing happens entirely
          on this machine — no timestamp server, no revocation lookup (those are
          optional online steps you can add later). Editing the document after
          signing will break the signature.
        </p>
        <button class="sign-pick" @click="choosePfx">
          <Icon name="lock" /> {{ signPfxPath ? signPfxName : "Choose digital ID (.pfx)…" }}
        </button>
        <input v-model="signPassword" type="password" placeholder="Digital ID password" />
        <input v-model="signName" type="text" placeholder="Name (optional)" />
        <input v-model="signReason" type="text" placeholder="Reason, e.g. “I approve this document” (optional)" />
        <input v-model="signLocation" type="text" placeholder="Location (optional)" />
        <p v-if="signError" class="error">{{ signError }}</p>
        <div class="protect-actions">
          <button type="button" @click="signOpen = false">Cancel</button>
          <span class="spacer"></span>
          <button class="primary" :disabled="signing || !signPfxPath" @click="doSign">
            {{ signing ? "Signing…" : "Sign &amp; save as…" }}
          </button>
        </div>
      </div>
    </div>

    <div v-if="tool === 'redact' && doc" class="redact-bar">
      <Icon name="square-dashed" />
      <span class="redact-msg">
        Drag boxes over anything to remove — {{ redactCount }} marked
      </span>
      <button class="ghost" :disabled="busy" @click="((redactScanOpen = true), (redactMatches = []))">
        <Icon name="shield-alert" /> Find sensitive data
      </button>
      <button class="ghost" :disabled="!redactCount || busy" @click="pendingRedactions = []">
        Clear
      </button>
      <button class="danger" :disabled="!redactCount || busy" @click="applyRedactions">
        Apply &amp; remove
      </button>
    </div>

    <div class="body">
      <aside v-if="toolsOpen" class="tools-panel">
        <p class="tools-title">All tools</p>
        <button class="tool-item" :disabled="!doc" @click="railOpen = !railOpen">
          <Icon name="layout-grid" /> Organize pages
          <span v-if="railOpen && doc" class="tool-state">shown</span>
        </button>
        <button class="tool-item" :disabled="!doc || busy" @click="mergeFiles">
          <Icon name="merge" /> Combine files
        </button>
        <button class="tool-item" :disabled="!doc || busy" @click="compressDoc">
          <Icon name="shrink" /> Compress a PDF
        </button>
        <button class="tool-item" :disabled="!doc || busy" @click="((protectOpen = true), (protectError = null))">
          <Icon :name="doc?.has_password ? 'lock' : 'lock-open'" /> Protect a PDF
          <span v-if="doc?.has_password" class="tool-state">on</span>
        </button>
        <button class="tool-item" :disabled="!doc || busy" @click="stampOpen = true">
          <Icon name="stamp" /> Watermark &amp; stamps
        </button>
        <button class="tool-item" :disabled="!doc || busy" @click="tool = tool === 'edit' ? 'select' : 'edit'">
          <Icon name="type" /> Edit a PDF
          <span v-if="tool === 'edit'" class="tool-state">on</span>
        </button>
        <button class="tool-item" :disabled="!doc || busy" @click="exportOpen = true">
          <Icon name="export" /> Export a PDF
        </button>
        <button class="tool-item" :disabled="busy" @click="createFromImages">
          <Icon name="image" /> Create from images
        </button>
        <button class="tool-item" :disabled="!doc || busy" @click="ocrOpen = true">
          <Icon name="scan-text" /> Scan &amp; OCR
        </button>
        <button class="tool-item" :disabled="!doc || busy" @click="tool = tool === 'redact' ? 'select' : 'redact'">
          <Icon name="square-dashed" /> Redact
          <span v-if="tool === 'redact'" class="tool-state">on</span>
        </button>
        <button class="tool-item" :disabled="!doc || busy" @click="((signOpen = true), (signError = null))">
          <Icon name="pen-tool" /> Sign
        </button>
      </aside>

      <aside v-if="doc && railOpen" ref="rail" class="rail">
        <div class="rail-head">
          <span v-if="!selected.size" class="rail-label">Pages</span>
          <template v-else>
            <span class="rail-label">{{ selected.size }} selected</span>
            <button class="iconbtn" :disabled="busy" title="Rotate left" aria-label="Rotate left" @click="rotateSelection(-1)"><Icon name="rotate-ccw" /></button>
            <button class="iconbtn" :disabled="busy" title="Rotate right" aria-label="Rotate right" @click="rotateSelection(1)"><Icon name="rotate-cw" /></button>
            <button class="iconbtn" :disabled="busy" title="Delete pages" aria-label="Delete pages" @click="deleteSelection"><Icon name="trash" /></button>
          </template>
        </div>
        <div
          v-for="(thumb, index) in thumbs"
          :key="index"
          :ref="(el) => observeThumb(el as Element | null, index)"
          class="thumb"
          :class="{
            selected: selected.has(index),
            dragover: dragOver === index && dragFrom !== null && dragFrom !== index,
          }"
          draggable="true"
          @click="toggleSelect(index, $event)"
          @dragstart="dragFrom = index"
          @dragover.prevent="dragOver = index"
          @dragleave="dragOver === index && (dragOver = null)"
          @drop.prevent="onDrop(index)"
          @dragend="((dragFrom = null), (dragOver = null))"
        >
          <div class="thumbpage" :style="{ height: thumbHeight(index) + 'px' }">
            <img v-if="thumb.url" :src="thumb.url" :alt="`Page ${index + 1}`" draggable="false" />
          </div>
          <span class="thumbnum">{{ index + 1 }}</span>
        </div>
      </aside>

      <div v-if="doc" class="quicktools">
        <button class="iconbtn" :class="{ active: tool === 'select' }" title="Select — click marks or form fields (Esc)" aria-label="Select tool"
          @click="((tool = 'select'), (selectedAnnot = null))"><Icon name="pointer" /></button>
        <button class="iconbtn" :class="{ active: tool === 'highlight' }" title="Highlight — drag over text" aria-label="Highlight tool"
          @click="tool = tool === 'highlight' ? 'select' : 'highlight'"><Icon name="highlighter" /></button>
        <button class="iconbtn" :class="{ active: tool === 'note' }" title="Note — click the page" aria-label="Note tool"
          @click="tool = tool === 'note' ? 'select' : 'note'"><Icon name="sticky-note" /></button>
        <button class="iconbtn" :class="{ active: tool === 'ink' }" title="Draw freehand (Ctrl+Z undoes)" aria-label="Draw tool"
          @click="tool = tool === 'ink' ? 'select' : 'ink'"><Icon name="pen" /></button>
        <button class="iconbtn" :class="{ active: tool === 'edit' }" title="Edit — click page text to rewrite it, images to delete" aria-label="Edit tool"
          @click="tool = tool === 'edit' ? 'select' : 'edit'"><Icon name="type" /></button>
        <button class="iconbtn" :class="{ active: tool === 'redact' }" title="Redact — drag boxes over content to remove it" aria-label="Redact tool"
          @click="tool = tool === 'redact' ? 'select' : 'redact'"><Icon name="square-dashed" /></button>
      </div>

      <div ref="scroller" class="well">
        <div v-if="needsPassword" class="notice">
          <p>This PDF is password protected.</p>
          <form @submit.prevent="submitPassword">
            <input v-model="password" type="password" placeholder="Password" autofocus />
            <button class="primary" type="submit">Unlock</button>
          </form>
          <p v-if="error" class="error">{{ error }}</p>
        </div>

        <div v-else-if="!doc" class="notice empty">
          <p class="brand">Zuga<span>PDF</span></p>
          <p>Open a PDF to get started. Everything happens on this machine.</p>
          <p v-if="error" class="error">{{ error }}</p>
        </div>

        <template v-else>
          <p v-if="error" class="error inline-error">{{ error }}</p>
          <p v-if="status" class="status inline-error">{{ status }}</p>
          <div
            v-for="(page, index) in pages"
            :key="index"
            :ref="(el) => observePage(el as Element | null, index)"
            class="page"
            :class="{ selected: selected.has(index), annotating: tool !== 'select' }"
            :style="{ width: cssWidth(index) + 'px', height: cssHeight(index) + 'px' }"
            @pointerdown="onPagePointerDown($event, index)"
            @pointermove="onPagePointerMove($event, index)"
            @pointerup="onPagePointerUp(index)"
          >
            <img v-if="page.url" :src="page.url" :alt="`Page ${index + 1}`" draggable="false" />
            <span v-else class="pagenum">{{ index + 1 }}</span>

            <div
              v-if="drag && drag.page === index && tool === 'highlight'"
              class="live-highlight"
              :style="{
                left: Math.min(drag.startX, drag.x) + 'px',
                top: Math.min(drag.startY, drag.y) + 'px',
                width: Math.abs(drag.x - drag.startX) + 'px',
                height: Math.abs(drag.y - drag.startY) + 'px',
              }"
            ></div>
            <svg v-if="drag && drag.page === index && tool === 'ink'" class="live-ink">
              <polyline :points="drag.points.map((p) => p.join(',')).join(' ')" />
            </svg>
            <div
              v-if="drag && drag.page === index && tool === 'redact'"
              class="live-redact"
              :style="{
                left: Math.min(drag.startX, drag.x) + 'px',
                top: Math.min(drag.startY, drag.y) + 'px',
                width: Math.abs(drag.x - drag.startX) + 'px',
                height: Math.abs(drag.y - drag.startY) + 'px',
              }"
            ></div>

            <template v-for="(r, ri) in pendingRedactions" :key="'r' + ri">
              <div v-if="r.page === index" class="pending-redact" :style="redactCss(r)">
                <button
                  v-if="tool === 'redact'"
                  class="annot-delete"
                  title="Remove this redaction mark"
                  @pointerdown.stop
                  @click.stop="removePendingRedaction(r)"
                ><Icon name="x" /></button>
              </div>
            </template>

            <template v-for="f in fieldsByPage.get(index) ?? []" :key="'f' + f.index">
              <div
                v-if="tool === 'select' && !(fieldEditor && fieldEditor.page === index && fieldEditor.index === f.index)"
                class="field-tint"
                :class="{ done: f.kind === 'Text' ? f.value.length > 0 : f.checked }"
                :style="fieldCss(f)"
                :title="f.name || f.kind"
              >
                <svg
                  v-if="f.checked && f.kind !== 'Text'"
                  class="checkmark"
                  viewBox="0 0 24 24"
                  aria-hidden="true"
                >
                  <path d="M4 13 L10 19 L20 6" />
                </svg>
              </div>
            </template>

            <input
              v-if="fieldEditor && fieldEditor.page === index"
              :key="fieldEditor.page + ':' + fieldEditor.index"
              :data-fid="fieldEditor.page + ':' + fieldEditor.index"
              class="field-editor"
              v-model="fieldEditor.value"
              :style="fieldEditor.css"
              v-focus
              @pointerdown.stop
              @keydown.enter.prevent="moveEditor(1)"
              @keydown.tab.exact.prevent="moveEditor(1)"
              @keydown.shift.tab.prevent="moveEditor(-1)"
              @keydown.escape="fieldEditor = null"
              @blur="onEditorBlur"
            />

            <template v-if="tool === 'edit'">
              <div
                v-for="o in objectsByPage.get(index) ?? []"
                :key="'o' + o.index"
                class="obj-outline"
                :class="{ image: o.kind === 'Image' }"
                :style="objCss(o)"
              ></div>
            </template>

            <input
              v-if="objEditor && objEditor.page === index"
              :key="'oe' + objEditor.page + ':' + objEditor.index"
              :data-oid="objEditor.page + ':' + objEditor.index"
              class="obj-editor"
              v-model="objEditor.value"
              :style="{ ...objEditor.css, fontSize: objEditor.fontPx + 'px' }"
              v-focus
              @pointerdown.stop
              @keydown.enter.prevent="submitObjEditor"
              @keydown.escape="objEditor = null"
              @blur="onObjEditorBlur"
            />

            <div
              v-if="selectedObj && selectedObj.page === index"
              class="annot-selection"
              :style="selectedObj.css"
            >
              <button
                class="annot-delete"
                :title="`Delete ${selectedObj.kind.toLowerCase()} (Del)`"
                @pointerdown.stop
                @click.stop="deleteSelectedObj"
              ><Icon name="x" /></button>
            </div>

            <template v-for="(h, hi) in searchHits" :key="'s' + hi">
              <div
                v-if="h.page === index"
                class="search-hit"
                :class="{ current: h.hit === searchCurrent }"
                :style="hitCss(h)"
              ></div>
            </template>

            <div
              v-if="selectedAnnot && selectedAnnot.page === index"
              class="annot-selection"
              :style="{
                left: selectedAnnot.css.left + 'px',
                top: selectedAnnot.css.top + 'px',
                width: selectedAnnot.css.width + 'px',
                height: selectedAnnot.css.height + 'px',
              }"
            >
              <button
                class="annot-delete"
                :title="`Delete ${selectedAnnot.kind.toLowerCase()} (Del)`"
                @pointerdown.stop
                @click.stop="deleteSelectedAnnot"
              ><Icon name="x" /></button>
            </div>

            <template v-for="mark in pendingMarks" :key="mark.key">
              <div
                v-if="mark.page === index && mark.rect"
                class="live-highlight"
                :style="{
                  left: mark.rect.left + 'px',
                  top: mark.rect.top + 'px',
                  width: mark.rect.width + 'px',
                  height: mark.rect.height + 'px',
                }"
              ></div>
              <svg v-if="mark.page === index && mark.points" class="live-ink">
                <polyline :points="mark.points.map((p) => p.join(',')).join(' ')" />
              </svg>
            </template>

            <form
              v-if="noteDraft && noteDraft.page === index"
              class="note-draft"
              :style="{ left: noteDraft.x + 'px', top: noteDraft.y + 'px' }"
              @submit.prevent="submitNote"
              @pointerdown.stop
            >
              <textarea
                v-model="noteDraft.text"
                rows="3"
                placeholder="Note text…"
                v-focus
                @keydown.enter.exact.prevent="submitNote"
                @keydown.escape="noteDraft = null"
              ></textarea>
              <div class="note-actions">
                <button type="button" @click="noteDraft = null">Cancel</button>
                <button class="primary" type="submit" :disabled="!noteDraft.text.trim()">
                  Add note
                </button>
              </div>
            </form>
          </div>
        </template>
      </div>
    </div>
  </div>
</template>

<style scoped>
.viewer {
  height: 100%;
  display: flex;
  flex-direction: column;
}
.toolbar {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  padding: 0.5rem 0.75rem;
  border-bottom: 1px solid var(--chrome-line);
  flex: none;
}
.divider {
  width: 1px;
  height: 1.4rem;
  background: var(--chrome-line);
}
.doctitle {
  font-weight: 600;
  font-size: 0.9rem;
  max-width: 20rem;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.doctitle i {
  color: var(--accent);
  font-style: normal;
}
.meta { color: var(--text-dim); font-size: 0.8rem; }
.spacer { flex: 1; }
.zoom {
  font-family: var(--mono);
  font-size: 0.8rem;
  min-width: 3.2rem;
  text-align: center;
  font-variant-numeric: tabular-nums;
}
.body {
  flex: 1;
  display: flex;
  min-height: 0;
  position: relative;
}
.tools-toggle {
  font-size: 0.85rem;
  font-weight: 600;
}
.tools-panel {
  flex: none;
  width: 15rem;
  border-right: 1px solid var(--chrome-line);
  padding: 0.9rem 0.75rem;
  display: flex;
  flex-direction: column;
  gap: 0.35rem;
  overflow-y: auto;
}
.tools-title {
  font-family: var(--mono);
  font-size: 0.7rem;
  letter-spacing: 0.1em;
  text-transform: uppercase;
  color: var(--text-dim);
  margin: 0 0 0.4rem 0.25rem;
}
.tool-item {
  display: flex;
  align-items: center;
  gap: 0.6rem;
  text-align: left;
  background: none;
  border: none;
  border-radius: 6px;
  padding: 0.5rem 0.6rem;
  font-size: 0.88rem;
}
.tool-item:hover:not(:disabled) { background: var(--chrome-raised); border: none; }
.tool-item .lucide { color: var(--accent); }
.tool-state {
  margin-left: auto;
  font-family: var(--mono);
  font-size: 0.65rem;
  color: var(--ok);
  text-transform: uppercase;
}
.tools-soon {
  margin: 0.75rem 0.25rem 0;
  font-size: 0.72rem;
  color: var(--text-dim);
  line-height: 1.5;
}
.quicktools {
  position: absolute;
  z-index: 10;
  top: 1rem;
  left: calc(100% - 4rem);
  display: flex;
  flex-direction: column;
  gap: 0.25rem;
  background: var(--chrome);
  border: 1px solid var(--chrome-line);
  border-radius: 8px;
  padding: 0.3rem;
  box-shadow: 0 4px 14px rgba(0, 0, 0, 0.4);
}
.rail-head {
  display: flex;
  align-items: center;
  gap: 0.25rem;
  min-height: 2.2rem;
  padding-bottom: 0.35rem;
}
.rail-label {
  flex: 1;
  font-family: var(--mono);
  font-size: 0.7rem;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--text-dim);
}
.rail {
  flex: none;
  width: 172px;
  overflow-y: auto;
  border-right: 1px solid var(--chrome-line);
  padding: 0.75rem;
  display: flex;
  flex-direction: column;
  gap: 0.6rem;
}
.thumb {
  border-radius: 6px;
  padding: 0.35rem 0.35rem 0.15rem;
  border: 2px solid transparent;
  cursor: pointer;
  text-align: center;
}
.thumb.selected { border-color: var(--accent); background: var(--chrome-raised); }
.thumb.dragover { border-color: var(--ok); }
.thumbpage {
  width: 132px;
  background: #fff;
  border-radius: 2px;
  box-shadow: 0 1px 5px rgba(0, 0, 0, 0.4);
  overflow: hidden;
}
.thumbpage img { width: 100%; height: 100%; display: block; }
.thumbnum {
  font-family: var(--mono);
  font-size: 0.7rem;
  color: var(--text-dim);
}
.well {
  flex: 1;
  overflow: auto;
  background: var(--well);
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 1.25rem;
  padding: 1.5rem;
}
.page {
  flex: none;
  position: relative;
  background: #fff;
  box-shadow: 0 2px 12px rgba(0, 0, 0, 0.45);
  border-radius: 2px;
  display: grid;
  place-items: center;
}
.page.annotating { cursor: crosshair; }
.page.annotating img { pointer-events: none; }
.live-highlight {
  position: absolute;
  background: rgba(255, 220, 60, 0.4);
  pointer-events: none;
}
.live-ink {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  pointer-events: none;
}
.live-ink polyline {
  fill: none;
  stroke: #2b4acb;
  stroke-width: 2.5;
  stroke-linecap: round;
  stroke-linejoin: round;
}
.live-redact {
  position: absolute;
  background: rgba(20, 20, 20, 0.55);
  outline: 1.5px solid var(--danger);
  pointer-events: none;
}
.pending-redact {
  position: absolute;
  background: #111;
  outline: 1.5px solid var(--danger);
  border-radius: 1px;
  pointer-events: none;
}
.pending-redact .annot-delete {
  top: -0.7rem;
  right: -0.7rem;
  width: 1.3rem;
  height: 1.3rem;
}
.note-draft {
  position: absolute;
  z-index: 5;
  background: var(--chrome);
  border: 1px solid var(--chrome-line);
  border-radius: 8px;
  padding: 0.5rem;
  width: 15rem;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.5);
}
.note-draft textarea {
  width: 100%;
  font: inherit;
  font-size: 0.85rem;
  background: var(--chrome-raised);
  color: var(--text);
  border: 1px solid var(--chrome-line);
  border-radius: 6px;
  padding: 0.35rem 0.5rem;
  resize: vertical;
}
.search {
  font: inherit;
  font-size: 0.85rem;
  width: 11rem;
  background: var(--chrome-raised);
  color: var(--text);
  border: 1px solid var(--chrome-line);
  border-radius: 6px;
  padding: 0.3rem 0.6rem;
}
.search:focus-visible { outline: 2px solid var(--accent); outline-offset: 1px; }
.iconbtn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 2.1rem;
  height: 2.1rem;
  padding: 0;
  font-size: 0.95rem;
}
.stamp-card { width: 26rem; }
.stamp-card hr { border: none; border-top: 1px solid var(--chrome-line); margin: 0.75rem 0 0.25rem; }
.stamp-row { display: flex; gap: 0.75rem; align-items: center; flex-wrap: wrap; }
.stamp-row label { display: inline-flex; align-items: center; gap: 0.35rem; font-size: 0.85rem; color: var(--text-dim); }
.stamp-row input[type="number"] { width: 4.5rem; }
.stamp-row input[type="text"] { flex: 1; }
.stamp-row select {
  font: inherit;
  background: var(--chrome-raised);
  color: var(--text);
  border: 1px solid var(--chrome-line);
  border-radius: 6px;
  padding: 0.35rem 0.5rem;
}
.protect-modal {
  position: fixed;
  inset: 0;
  z-index: 20;
  background: rgba(10, 13, 20, 0.6);
  display: grid;
  place-items: center;
}
.protect-card {
  background: var(--chrome);
  border: 1px solid var(--chrome-line);
  border-radius: 10px;
  padding: 1.5rem 1.75rem;
  width: 24rem;
  display: flex;
  flex-direction: column;
  gap: 0.6rem;
}
.protect-card h2 { font-size: 1.05rem; margin: 0; font-family: var(--sans); }
.protect-hint { color: var(--text-dim); font-size: 0.82rem; margin: 0 0 0.4rem; }
.protect-card input {
  font: inherit;
  background: var(--chrome-raised);
  color: var(--text);
  border: 1px solid var(--chrome-line);
  border-radius: 6px;
  padding: 0.4rem 0.6rem;
}
.protect-actions { display: flex; gap: 0.5rem; margin-top: 0.5rem; align-items: center; }
.protect-actions .spacer { flex: 1; }
button.danger { color: var(--danger); }

/* redaction */
.redact-bar {
  position: fixed;
  bottom: 1.1rem;
  left: 50%;
  transform: translateX(-50%);
  z-index: 15;
  display: flex;
  align-items: center;
  gap: 0.5rem;
  padding: 0.4rem 0.6rem 0.4rem 0.75rem;
  background: var(--chrome);
  border: 1px solid var(--chrome-line);
  border-radius: 10px;
  box-shadow: 0 6px 22px rgba(0, 0, 0, 0.35);
  font-size: 0.85rem;
}
.redact-bar .redact-msg { color: var(--text-dim); margin-right: 0.25rem; }
.redact-bar button {
  display: inline-flex;
  align-items: center;
  gap: 0.3rem;
}
.redact-bar button.danger {
  border-color: var(--danger);
  font-weight: 600;
}
.scan-card { width: 27rem; max-height: 80vh; overflow-y: auto; }
.scan-kinds { display: flex; flex-direction: column; gap: 0.3rem; }
.scan-kinds label,
.scan-results label {
  display: flex;
  align-items: center;
  gap: 0.45rem;
  font-size: 0.85rem;
  cursor: pointer;
}
.scan-card input[type="checkbox"] {
  width: auto;
  padding: 0;
  margin: 0;
  accent-color: var(--accent);
}
.scan-run { align-self: flex-start; }
.net-toggle {
  display: flex;
  align-items: flex-start;
  gap: 0.7rem;
  padding: 0.55rem 0;
  border-top: 1px solid var(--chrome-line);
}
.net-copy { display: flex; flex-direction: column; gap: 0.15rem; }
.net-copy strong { font-size: 0.9rem; }
.net-copy span { font-size: 0.78rem; color: var(--text-dim); line-height: 1.35; }
.net-foot { font-size: 0.75rem; color: var(--text-dim); margin: 0.3rem 0 0; }
.switch { position: relative; flex: none; width: 2.4rem; height: 1.35rem; cursor: pointer; margin-top: 0.1rem; }
.switch input { position: absolute; opacity: 0; width: 0; height: 0; }
.switch-track {
  position: absolute;
  inset: 0;
  border-radius: 999px;
  background: var(--chrome-line);
  transition: background 0.15s;
}
.switch-track::after {
  content: "";
  position: absolute;
  top: 0.15rem;
  left: 0.15rem;
  width: 1.05rem;
  height: 1.05rem;
  border-radius: 50%;
  background: #fff;
  transition: transform 0.15s;
}
.switch input:checked + .switch-track { background: var(--accent); }
.switch input:checked + .switch-track::after { transform: translateX(1.05rem); }
.switch input:focus-visible + .switch-track { outline: 2px solid var(--accent); outline-offset: 2px; }
.sign-pick {
  display: inline-flex;
  align-items: center;
  gap: 0.45rem;
  justify-content: flex-start;
  padding: 0.45rem 0.6rem;
  border: 1px dashed var(--chrome-line);
  border-radius: 6px;
  background: var(--chrome-raised);
  color: var(--text);
}
.scan-results { border-top: 1px solid var(--chrome-line); padding-top: 0.5rem; }
.scan-count { font-size: 0.8rem; color: var(--text-dim); margin: 0 0 0.4rem; }
.scan-results ul { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 0.2rem; }
.scan-kind {
  font-size: 0.68rem;
  text-transform: uppercase;
  letter-spacing: 0.03em;
  color: var(--danger);
  min-width: 3.2rem;
}
.scan-sample { font-family: var(--mono, monospace); flex: 1; }
.scan-page { color: var(--text-dim); font-size: 0.75rem; }
.field-tint {
  position: absolute;
  background: rgba(80, 130, 240, 0.16);
  outline: 1px dashed rgba(80, 130, 240, 0.55);
  border-radius: 2px;
  cursor: text;
  pointer-events: none;
}
.field-tint.done { background: rgba(80, 200, 130, 0.12); outline-color: rgba(80, 200, 130, 0.5); }
.field-tint .checkmark {
  width: 100%;
  height: 100%;
  display: block;
  padding: 8%;
  box-sizing: border-box;
}
.field-tint .checkmark path {
  fill: none;
  stroke: #14652f;
  stroke-width: 3.5;
  stroke-linecap: round;
  stroke-linejoin: round;
}
.field-editor {
  position: absolute;
  z-index: 6;
  font: inherit;
  background: #fff;
  color: #111;
  border: 2px solid var(--accent);
  border-radius: 2px;
  padding: 0 0.2rem;
}
.obj-outline {
  position: absolute;
  outline: 1px dashed rgba(125, 149, 242, 0.55);
  border-radius: 2px;
  pointer-events: none;
}
.obj-outline.image { outline-color: rgba(230, 181, 104, 0.6); }
.obj-editor {
  position: absolute;
  z-index: 6;
  font-family: Helvetica, Arial, sans-serif;
  background: #fff;
  color: #111;
  border: 2px solid var(--accent);
  border-radius: 2px;
  padding: 0;
  min-width: 3rem;
}
.search-hit {
  position: absolute;
  background: rgba(255, 150, 40, 0.35);
  outline: 1px solid rgba(255, 150, 40, 0.7);
  border-radius: 2px;
  pointer-events: none;
}
.search-hit.current {
  background: rgba(255, 110, 40, 0.55);
  outline: 2px solid #ff7a28;
}
.annot-selection {
  position: absolute;
  border: 2px dashed var(--accent);
  border-radius: 3px;
  pointer-events: none;
}
.annot-delete {
  position: absolute;
  top: -0.85rem;
  right: -0.85rem;
  pointer-events: auto;
  width: 1.5rem;
  height: 1.5rem;
  padding: 0;
  display: grid;
  place-items: center;
  border-radius: 50%;
  background: var(--chrome);
  color: var(--danger);
  font-size: 0.8rem;
}
.note-actions {
  display: flex;
  justify-content: flex-end;
  gap: 0.4rem;
  margin-top: 0.4rem;
}
button.active {
  border-color: var(--accent);
  color: var(--accent);
}
.page.selected { outline: 3px solid var(--accent); }
.page img {
  width: 100%;
  height: 100%;
  border-radius: 2px;
}
.pagenum {
  font-family: var(--mono);
  color: #b9bdc7;
  font-size: 1.5rem;
}
.notice {
  margin: auto;
  text-align: center;
  color: var(--text-dim);
  max-width: 26rem;
}
.notice form { display: flex; gap: 0.5rem; justify-content: center; }
.notice input {
  font: inherit;
  background: var(--chrome-raised);
  color: var(--text);
  border: 1px solid var(--chrome-line);
  border-radius: 6px;
  padding: 0.35rem 0.6rem;
}
.brand {
  font-family: var(--serif);
  font-size: 2.2rem;
  color: var(--text);
  margin-bottom: 0.25rem;
}
.brand span { color: var(--accent); }
.empty p:last-child { font-size: 0.9rem; }
.error { color: var(--danger); }
.status { color: var(--ok); }
.inline-error { margin: 0; font-size: 0.85rem; }
</style>
