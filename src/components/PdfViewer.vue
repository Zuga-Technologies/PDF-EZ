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
        }
      }
    },
    { root, rootMargin: "600px 0px" },
  );
}

async function renderSlot(slots: RenderSlot[], index: number, want: number) {
  if (!doc.value) return;
  const slot = slots[index];
  if (!slot || slot.loading || (slot.url && slot.renderedWidth === want)) return;
  slot.loading = true;
  try {
    const rendered = await api.renderPage(doc.value.id, index, want);
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
    void renderSlot(pages, index, want).then(() => {
      // Zoom may have moved again while this render was in flight.
      const now = Math.round(cssWidth(index) * dpr());
      if (visiblePages.has(index) && pages[index] && pages[index].renderedWidth !== now) {
        renderPageIfStale(index);
      }
    });
  }
}
function renderThumbIfStale(index: number) {
  void renderSlot(thumbs, index, Math.round(THUMB_CSS_WIDTH * dpr()));
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
  if (e.ctrlKey && e.key.toLowerCase() === "s") {
    e.preventDefault();
    e.shiftKey ? void saveAs() : void saveNow();
  } else if (e.key === "Delete" && selected.value.size) {
    void deleteSelection();
  }
}

onMounted(() => {
  pageObserver = makeObserver(scroller.value, visiblePages, renderPageIfStale);
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
            :class="{ selected: selected.has(index) }"
            :style="{ width: cssWidth(index) + 'px', height: cssHeight(index) + 'px' }"
          >
            <img v-if="page.url" :src="page.url" :alt="`Page ${index + 1}`" draggable="false" />
            <span v-else class="pagenum">{{ index + 1 }}</span>
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
  background: #fff;
  box-shadow: 0 2px 12px rgba(0, 0, 0, 0.45);
  border-radius: 2px;
  display: grid;
  place-items: center;
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
