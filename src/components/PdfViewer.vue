<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from "vue";
import { confirm, open, save } from "@tauri-apps/plugin-dialog";
import { api, type DocInfo } from "../lib/api";

interface RenderSlot {
  url: string | null;
  renderedWidth: number; // device px the current bitmap was rendered at
  loading: boolean;
}

const doc = ref<DocInfo | null>(null);
const pages = reactive<RenderSlot[]>([]);
const thumbs = reactive<RenderSlot[]>([]);
const selected = ref<Set<number>>(new Set());
const dirty = ref(false);
const busy = ref(false);
const error = ref<string | null>(null);
const zoom = ref(1.0);
const needsPassword = ref(false);
const password = ref("");
const pendingPath = ref<string | null>(null);
const dragFrom = ref<number | null>(null);
const dragOver = ref<number | null>(null);

const ZOOM_STEPS = [0.5, 0.67, 0.8, 1.0, 1.25, 1.5, 2.0, 3.0];
const POINTS_TO_CSS = 96 / 72;
const THUMB_CSS_WIDTH = 132;

// ---- annotation tools ----

type Tool = "select" | "highlight" | "note" | "ink";
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
  undoStack.length = 0; // annotation indices are stale after any page op
  // Already-visible elements get no new intersection event; refresh them.
  for (const index of visiblePages) renderPageIfStale(index);
  for (const index of visibleThumbs) renderThumbIfStale(index);
}

async function confirmDiscard(): Promise<boolean> {
  if (!dirty.value) return true;
  return confirm("You have unsaved changes. Discard them?", {
    title: "ZugaPDF",
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
  needsPassword.value = false;
  try {
    if (doc.value) {
      const old = doc.value.id;
      doc.value = null;
      void api.closeDocument(old);
    }
    const info = await api.openDocument(path, pw);
    dirty.value = false;
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
  if (!(await confirm(`Delete ${noun}?`, { title: "ZugaPDF", kind: "warning" }))) return;
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
    void selectAnnotAt(event, index);
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
  }
}

async function selectAnnotAt(event: PointerEvent, index: number) {
  if (!doc.value) return;
  const [cssX, cssY] = pagePoint(event);
  const [px, py] = cssToPdf(index, cssX, cssY);
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
  if (noteDraft.value) return; // typing in the note box
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
      <button class="primary" @click="openFile">Open PDF…</button>
      <template v-if="doc">
        <button :disabled="!dirty || busy" title="Ctrl+S" @click="saveNow">Save</button>
        <button :disabled="busy" title="Ctrl+Shift+S" @click="saveAs">Save as…</button>
        <span class="divider"></span>
        <button
          :disabled="!selected.size || busy"
          title="Rotate left"
          @click="rotateSelection(-1)"
        >⟲</button>
        <button
          :disabled="!selected.size || busy"
          title="Rotate right"
          @click="rotateSelection(1)"
        >⟳</button>
        <button :disabled="!selected.size || busy" title="Delete pages" @click="deleteSelection">
          Delete
        </button>
        <span class="divider"></span>
        <button :class="{ active: tool === 'select' }" title="Click any highlight, note, or drawing to select and delete it (Esc)"
          @click="((tool = 'select'), (selectedAnnot = null))">Select</button>
        <button :class="{ active: tool === 'highlight' }" title="Drag over text to highlight"
          @click="tool = tool === 'highlight' ? 'select' : 'highlight'">Highlight</button>
        <button :class="{ active: tool === 'note' }" title="Click the page to leave a note"
          @click="tool = tool === 'note' ? 'select' : 'note'">Note</button>
        <button :class="{ active: tool === 'ink' }" title="Draw freehand (Ctrl+Z undoes)"
          @click="tool = tool === 'ink' ? 'select' : 'ink'">Draw</button>
      </template>
      <div class="spacer"></div>
      <template v-if="doc">
        <span class="doctitle" :title="doc.path">{{ title }}<i v-if="dirty"> •</i></span>
        <span class="meta">{{ doc.page_count }} {{ doc.page_count === 1 ? "page" : "pages" }}</span>
        <button aria-label="Zoom out" @click="zoomOut">−</button>
        <span class="zoom">{{ Math.round(zoom * 100) }}%</span>
        <button aria-label="Zoom in" @click="zoomIn">+</button>
      </template>
    </header>

    <div class="body">
      <aside v-if="doc" ref="rail" class="rail">
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
              >×</button>
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
                autofocus
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
  line-height: 1;
  border-radius: 50%;
  background: var(--chrome);
  color: var(--danger);
  font-size: 1rem;
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
.inline-error { margin: 0; font-size: 0.85rem; }
</style>
