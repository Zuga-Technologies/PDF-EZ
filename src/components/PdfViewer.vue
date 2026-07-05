<script setup lang="ts">
import { computed, onBeforeUnmount, reactive, ref, watch } from "vue";
import { open } from "@tauri-apps/plugin-dialog";
import { api, type DocInfo } from "../lib/api";

interface PageState {
  url: string | null;
  renderedWidth: number; // device px the current bitmap was rendered at
  loading: boolean;
}

const doc = ref<DocInfo | null>(null);
const docPath = ref<string | null>(null);
const pages = reactive<PageState[]>([]);
const error = ref<string | null>(null);
const zoom = ref(1.0);
const needsPassword = ref(false);
const password = ref("");

const ZOOM_STEPS = [0.5, 0.67, 0.8, 1.0, 1.25, 1.5, 2.0, 3.0];
const POINTS_TO_CSS = 96 / 72;

const title = computed(() => {
  if (!doc.value) return "";
  if (doc.value.title) return doc.value.title;
  const p = docPath.value ?? "";
  return p.split(/[\\/]/).pop() ?? "";
});

// CSS width per page at the current zoom.
function cssWidth(index: number): number {
  const dims = doc.value!.pages[index];
  return Math.round(dims.width * POINTS_TO_CSS * zoom.value);
}
function cssHeight(index: number): number {
  const dims = doc.value!.pages[index];
  return Math.round(dims.height * POINTS_TO_CSS * zoom.value);
}
// Device-pixel width we ask the engine for (crisp on HiDPI).
function targetWidth(index: number): number {
  return Math.round(cssWidth(index) * (window.devicePixelRatio || 1));
}

const visible = new Set<number>();
let observer: IntersectionObserver | null = null;

function observe(el: Element | null, index: number) {
  if (!el || !observer) return;
  (el as HTMLElement).dataset.page = String(index);
  observer.observe(el);
}

function setupObserver() {
  observer?.disconnect();
  visible.clear();
  observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        const index = Number((entry.target as HTMLElement).dataset.page);
        if (entry.isIntersecting) {
          visible.add(index);
          void renderIfStale(index);
        } else {
          visible.delete(index);
        }
      }
    },
    { root: scroller.value, rootMargin: "600px 0px" },
  );
}

const scroller = ref<HTMLElement | null>(null);

async function renderIfStale(index: number) {
  if (!doc.value) return;
  const page = pages[index];
  const want = targetWidth(index);
  if (page.loading || (page.url && page.renderedWidth === want)) return;
  page.loading = true;
  try {
    const rendered = await api.renderPage(doc.value.id, index, want);
    page.url = rendered.data_url;
    page.renderedWidth = want;
  } catch (e) {
    error.value = String(e);
  } finally {
    page.loading = false;
    // Zoom may have moved again while this render was in flight.
    if (visible.has(index) && targetWidth(index) !== page.renderedWidth) {
      void renderIfStale(index);
    }
  }
}

watch(zoom, () => {
  for (const index of visible) void renderIfStale(index);
});

async function openFile() {
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
    docPath.value = path;
    doc.value = info;
    pages.length = 0;
    for (let i = 0; i < info.page_count; i++) {
      pages.push({ url: null, renderedWidth: 0, loading: false });
    }
    setupObserver();
  } catch (e) {
    if (String(e).includes("PASSWORD_REQUIRED")) {
      docPath.value = path;
      needsPassword.value = true;
      if (pw) error.value = "Wrong password — try again.";
    } else {
      error.value = String(e);
    }
  }
}

function submitPassword() {
  if (docPath.value) void loadDocument(docPath.value, password.value);
}

function zoomIn() {
  const next = ZOOM_STEPS.find((z) => z > zoom.value + 0.001);
  if (next) zoom.value = next;
}
function zoomOut() {
  const next = [...ZOOM_STEPS].reverse().find((z) => z < zoom.value - 0.001);
  if (next) zoom.value = next;
}

onBeforeUnmount(() => {
  observer?.disconnect();
  if (doc.value) void api.closeDocument(doc.value.id);
});
</script>

<template>
  <div class="viewer">
    <header class="toolbar">
      <button class="primary" @click="openFile">Open PDF…</button>
      <span v-if="doc" class="doctitle" :title="docPath ?? ''">{{ title }}</span>
      <span v-if="doc" class="meta">{{ doc.page_count }} {{ doc.page_count === 1 ? "page" : "pages" }}</span>
      <div class="spacer"></div>
      <template v-if="doc">
        <button aria-label="Zoom out" @click="zoomOut">−</button>
        <span class="zoom">{{ Math.round(zoom * 100) }}%</span>
        <button aria-label="Zoom in" @click="zoomIn">+</button>
      </template>
    </header>

    <div ref="scroller" class="well">
      <div v-if="needsPassword" class="notice">
        <p>This PDF is password protected.</p>
        <form @submit.prevent="submitPassword">
          <input v-model="password" type="password" placeholder="Password" autofocus />
          <button class="primary" type="submit">Unlock</button>
        </form>
        <p v-if="error" class="error">{{ error }}</p>
      </div>

      <div v-else-if="error" class="notice">
        <p class="error">{{ error }}</p>
      </div>

      <div v-else-if="!doc" class="notice empty">
        <p class="brand">Zuga<span>PDF</span></p>
        <p>Open a PDF to get started. Everything happens on this machine.</p>
      </div>

      <template v-else>
        <div
          v-for="(page, index) in pages"
          :key="index"
          :ref="(el) => observe(el as Element | null, index)"
          class="page"
          :style="{ width: cssWidth(index) + 'px', height: cssHeight(index) + 'px' }"
        >
          <img v-if="page.url" :src="page.url" :alt="`Page ${index + 1}`" draggable="false" />
          <span v-else class="pagenum">{{ index + 1 }}</span>
        </div>
      </template>
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
  gap: 0.6rem;
  padding: 0.5rem 0.75rem;
  border-bottom: 1px solid var(--chrome-line);
  flex: none;
}
.doctitle {
  font-weight: 600;
  font-size: 0.9rem;
  max-width: 22rem;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
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
</style>
