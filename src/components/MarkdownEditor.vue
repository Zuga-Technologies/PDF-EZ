<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { confirm, open, save } from "@tauri-apps/plugin-dialog";
import { useEditor, EditorContent } from "@tiptap/vue-3";
import StarterKit from "@tiptap/starter-kit";
import { Markdown } from "tiptap-markdown";
import Table from "@tiptap/extension-table";
import TableRow from "@tiptap/extension-table-row";
import TableCell from "@tiptap/extension-table-cell";
import TableHeader from "@tiptap/extension-table-header";
import TaskList from "@tiptap/extension-task-list";
import TaskItem from "@tiptap/extension-task-item";
import CodeBlockLowlight from "@tiptap/extension-code-block-lowlight";
import { createLowlight, common } from "lowlight";
import ImageExt from "@tiptap/extension-image";
import LinkExt from "@tiptap/extension-link";
import Placeholder from "@tiptap/extension-placeholder";
import { api } from "../lib/api";
import Icon from "./Icon.vue";

const props = defineProps<{ openPath?: string }>();
const emit = defineEmits<{ (e: "switch-kind", path: string): void }>();

const railOpen = ref(true);
const filePath = ref<string | null>(null);
const dirty = ref(false);
const busy = ref(false);
const error = ref<string | null>(null);
const status = ref<string | null>(null);

interface Heading {
  level: number;
  text: string;
  pos: number;
}
const headings = ref<Heading[]>([]);

function recomputeHeadings() {
  const ed = editor.value;
  if (!ed) {
    headings.value = [];
    return;
  }
  const list: Heading[] = [];
  ed.state.doc.descendants((node, pos) => {
    if (node.type.name === "heading") {
      list.push({ level: node.attrs.level as number, text: node.textContent || "(untitled)", pos });
    }
    return true;
  });
  headings.value = list;
}

function scrollToHeading(pos: number) {
  const dom = editor.value?.view.nodeDOM(pos) as HTMLElement | null | undefined;
  dom?.scrollIntoView({ behavior: "smooth", block: "start" });
}

const lowlight = createLowlight(common);

const editor = useEditor({
  content: "",
  extensions: [
    StarterKit.configure({ codeBlock: false, heading: { levels: [1, 2, 3] } }),
    CodeBlockLowlight.configure({ lowlight }),
    TaskList,
    TaskItem.configure({ nested: true }),
    Table.configure({ resizable: true }),
    TableRow,
    TableHeader,
    TableCell,
    ImageExt,
    LinkExt.configure({ openOnClick: false, autolink: true }),
    Placeholder.configure({ placeholder: "Start writing…" }),
    Markdown.configure({ html: false, transformPastedText: true, transformCopiedText: true }),
  ],
  editorProps: { attributes: { class: "md-prose" } },
  onUpdate: () => {
    dirty.value = true;
    recomputeHeadings();
  },
});

const title = computed(() => {
  const p = filePath.value ?? "";
  return p.split(/[\\/]/).pop() ?? "Untitled.md";
});

// ---- document lifecycle ----

async function confirmDiscard(): Promise<boolean> {
  if (!dirty.value) return true;
  return confirm("You have unsaved changes. Discard them?", { title: "PDF-EZ", kind: "warning" });
}

async function loadDocument(path: string) {
  error.value = null;
  status.value = null;
  busy.value = true;
  try {
    const text = await api.readTextFile(path);
    editor.value?.commands.setContent(text, false);
    filePath.value = path;
    dirty.value = false;
    recomputeHeadings();
  } catch (e) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function openFile() {
  if (!(await confirmDiscard())) return;
  const picked = await open({
    title: "Open document",
    filters: [{ name: "Documents", extensions: ["pdf", "md", "markdown"] }],
    multiple: false,
  });
  if (typeof picked !== "string") return;
  if (/\.pdf$/i.test(picked)) {
    emit("switch-kind", picked);
    return;
  }
  await loadDocument(picked);
}

async function saveNow() {
  if (!editor.value || busy.value) return;
  if (!filePath.value) return saveAs();
  busy.value = true;
  error.value = null;
  try {
    await api.writeTextFile(filePath.value, editor.value.storage.markdown.getMarkdown());
    dirty.value = false;
    status.value = "Saved";
  } catch (e) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function saveAs() {
  if (!editor.value || busy.value) return;
  const target = await save({
    title: "Save Markdown as",
    defaultPath: title.value,
    filters: [{ name: "Markdown", extensions: ["md"] }],
  });
  if (!target) return;
  busy.value = true;
  error.value = null;
  try {
    await api.writeTextFile(target, editor.value.storage.markdown.getMarkdown());
    filePath.value = target;
    dirty.value = false;
    status.value = "Saved";
  } catch (e) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

// ---- link popover ----
const linkPromptOpen = ref(false);
const linkUrl = ref("");

function openLinkPrompt() {
  linkUrl.value = (editor.value?.getAttributes("link").href as string) ?? "";
  linkPromptOpen.value = true;
}

function applyLink() {
  const ed = editor.value;
  linkPromptOpen.value = false;
  if (!ed) return;
  const url = linkUrl.value.trim();
  const chain = ed.chain().focus().extendMarkRange("link");
  if (!url) chain.unsetLink().run();
  else chain.setLink({ href: url }).run();
}

// ---- image insert ----
async function insertImage() {
  const picked = await open({
    title: "Insert image",
    filters: [{ name: "Images", extensions: ["png", "jpg", "jpeg", "gif", "webp", "svg"] }],
    multiple: false,
  });
  if (typeof picked !== "string") return;
  error.value = null;
  try {
    const b64 = await api.readFileBase64(picked);
    const ext = picked.split(".").pop()?.toLowerCase() ?? "png";
    const mime =
      ext === "svg" ? "image/svg+xml" :
      ext === "gif" ? "image/gif" :
      ext === "jpg" || ext === "jpeg" ? "image/jpeg" :
      ext === "webp" ? "image/webp" : "image/png";
    editor.value?.chain().focus().setImage({ src: `data:${mime};base64,${b64}` }).run();
  } catch (e) {
    error.value = String(e);
  }
}

// ---- table insert ----
function insertTable() {
  editor.value?.chain().focus().insertTable({ rows: 3, cols: 3, withHeaderRow: true }).run();
}

// ---- keyboard ----
function onKeydown(e: KeyboardEvent) {
  if (linkPromptOpen.value) return;
  if (e.ctrlKey && e.key.toLowerCase() === "s") {
    e.preventDefault();
    e.shiftKey ? void saveAs() : void saveNow();
  }
}

onMounted(() => {
  window.addEventListener("keydown", onKeydown);
  if (props.openPath) {
    void loadDocument(props.openPath);
  } else {
    void api.getLaunchFile().then((path) => {
      if (path && /\.(md|markdown)$/i.test(path)) void loadDocument(path);
    });
  }
});

onBeforeUnmount(() => {
  window.removeEventListener("keydown", onKeydown);
});
</script>

<template>
  <div class="viewer">
    <header class="toolbar">
      <button
        class="tools-toggle"
        :class="{ active: railOpen }"
        title="Show or hide the outline"
        @click="railOpen = !railOpen"
      >
        Outline
      </button>
      <button class="iconbtn primary" title="Open a document" aria-label="Open" @click="openFile">
        <Icon name="folder-open" />
      </button>
      <template v-if="editor">
        <button class="iconbtn" :disabled="!dirty || busy" title="Save (Ctrl+S)" aria-label="Save" @click="saveNow">
          <Icon name="save" />
        </button>
        <button class="iconbtn" :disabled="busy" title="Save as… (Ctrl+Shift+S)" aria-label="Save as" @click="saveAs">
          <Icon name="save-all" />
        </button>
        <span class="divider"></span>
        <button
          class="iconbtn"
          title="Undo"
          aria-label="Undo"
          :disabled="!editor.can().undo()"
          @click="editor.chain().focus().undo().run()"
        >
          <Icon name="undo-2" />
        </button>
        <button
          class="iconbtn"
          title="Redo"
          aria-label="Redo"
          :disabled="!editor.can().redo()"
          @click="editor.chain().focus().redo().run()"
        >
          <Icon name="redo-2" />
        </button>
        <span class="divider"></span>
        <button
          class="iconbtn"
          :class="{ active: editor.isActive('bold') }"
          title="Bold (Ctrl+B)"
          @click="editor.chain().focus().toggleBold().run()"
        >
          <Icon name="bold" />
        </button>
        <button
          class="iconbtn"
          :class="{ active: editor.isActive('italic') }"
          title="Italic (Ctrl+I)"
          @click="editor.chain().focus().toggleItalic().run()"
        >
          <Icon name="italic" />
        </button>
        <button
          class="iconbtn"
          :class="{ active: editor.isActive('strike') }"
          title="Strikethrough"
          @click="editor.chain().focus().toggleStrike().run()"
        >
          <Icon name="strikethrough" />
        </button>
        <button
          class="iconbtn"
          :class="{ active: editor.isActive('code') }"
          title="Inline code"
          @click="editor.chain().focus().toggleCode().run()"
        >
          <Icon name="code" />
        </button>
        <span class="divider"></span>
        <button
          class="hbtn"
          :class="{ active: editor.isActive('heading', { level: 1 }) }"
          title="Heading 1"
          @click="editor.chain().focus().toggleHeading({ level: 1 }).run()"
        >
          H1
        </button>
        <button
          class="hbtn"
          :class="{ active: editor.isActive('heading', { level: 2 }) }"
          title="Heading 2"
          @click="editor.chain().focus().toggleHeading({ level: 2 }).run()"
        >
          H2
        </button>
        <button
          class="hbtn"
          :class="{ active: editor.isActive('heading', { level: 3 }) }"
          title="Heading 3"
          @click="editor.chain().focus().toggleHeading({ level: 3 }).run()"
        >
          H3
        </button>
        <span class="divider"></span>
        <button
          class="iconbtn"
          :class="{ active: editor.isActive('bulletList') }"
          title="Bullet list"
          @click="editor.chain().focus().toggleBulletList().run()"
        >
          <Icon name="list" />
        </button>
        <button
          class="iconbtn"
          :class="{ active: editor.isActive('orderedList') }"
          title="Numbered list"
          @click="editor.chain().focus().toggleOrderedList().run()"
        >
          <Icon name="list-ordered" />
        </button>
        <button
          class="iconbtn"
          :class="{ active: editor.isActive('taskList') }"
          title="Checklist"
          @click="editor.chain().focus().toggleTaskList().run()"
        >
          <Icon name="list-todo" />
        </button>
        <button
          class="iconbtn"
          :class="{ active: editor.isActive('blockquote') }"
          title="Quote"
          @click="editor.chain().focus().toggleBlockquote().run()"
        >
          <Icon name="quote" />
        </button>
        <button
          class="iconbtn"
          :class="{ active: editor.isActive('codeBlock') }"
          title="Code block"
          @click="editor.chain().focus().toggleCodeBlock().run()"
        >
          <Icon name="code" />
        </button>
        <span class="divider"></span>
        <button class="iconbtn" :class="{ active: editor.isActive('link') }" title="Link" @click="openLinkPrompt">
          <Icon name="link" />
        </button>
        <button class="iconbtn" title="Insert image" @click="insertImage">
          <Icon name="image" />
        </button>
        <button class="iconbtn" title="Insert table" @click="insertTable">
          <Icon name="table" />
        </button>
      </template>
      <div class="spacer"></div>
      <template v-if="filePath">
        <span class="doctitle" :title="filePath">{{ title }}<i v-if="dirty"> •</i></span>
      </template>
    </header>

    <p v-if="error" class="inline-error error">{{ error }}</p>
    <p v-else-if="status" class="inline-error status">{{ status }}</p>

    <div class="body">
      <aside v-if="railOpen" class="rail">
        <div class="rail-head"><span class="rail-label">Outline</span></div>
        <p v-if="!headings.length" class="rail-empty">Headings you add will show up here.</p>
        <button
          v-for="h in headings"
          :key="h.pos"
          class="outline-item"
          :style="{ paddingLeft: 0.5 + (h.level - 1) * 0.85 + 'rem' }"
          @click="scrollToHeading(h.pos)"
        >
          {{ h.text }}
        </button>
      </aside>

      <div class="scroller">
        <div v-if="!filePath" class="empty">
          <p>Open a Markdown file to start writing.</p>
          <button class="primary" @click="openFile">Open…</button>
        </div>
        <div v-else class="md-page">
          <EditorContent :editor="editor" />
        </div>
      </div>
    </div>

    <div v-if="linkPromptOpen" class="protect-modal" @pointerdown.self="linkPromptOpen = false">
      <div class="protect-card link-card">
        <h2>Link</h2>
        <input v-model="linkUrl" type="text" placeholder="https://…" @keydown.enter.prevent="applyLink" />
        <div class="protect-actions">
          <button type="button" @click="linkPromptOpen = false">Cancel</button>
          <span class="spacer"></span>
          <button class="primary" @click="applyLink">{{ linkUrl.trim() ? "Apply" : "Remove link" }}</button>
        </div>
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
  gap: 0.35rem;
  padding: 0.5rem 0.75rem;
  border-bottom: 1px solid var(--chrome-line);
  flex: none;
  flex-wrap: wrap;
}
.tools-toggle {
  font-size: 0.85rem;
  font-weight: 600;
}
.divider {
  width: 1px;
  height: 1.4rem;
  background: var(--chrome-line);
}
.iconbtn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 2.1rem;
  height: 2.1rem;
  padding: 0;
  font-size: 0.95rem;
}
.iconbtn.active,
.tools-toggle.active {
  border-color: var(--accent);
  color: var(--accent);
}
.hbtn {
  width: 2.1rem;
  height: 2.1rem;
  padding: 0;
  font-family: var(--mono);
  font-size: 0.75rem;
  font-weight: 700;
}
.hbtn.active {
  border-color: var(--accent);
  color: var(--accent);
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
.spacer {
  flex: 1;
}
.error {
  color: var(--danger);
}
.status {
  color: var(--ok);
}
.inline-error {
  margin: 0;
  padding: 0.35rem 0.9rem;
  font-size: 0.85rem;
  border-bottom: 1px solid var(--chrome-line);
}
.body {
  flex: 1;
  display: flex;
  min-height: 0;
  position: relative;
}
.rail {
  flex: none;
  width: 200px;
  overflow-y: auto;
  border-right: 1px solid var(--chrome-line);
  padding: 0.75rem;
  display: flex;
  flex-direction: column;
  gap: 0.15rem;
}
.rail-head {
  display: flex;
  align-items: center;
  min-height: 1.6rem;
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
.rail-empty {
  color: var(--text-dim);
  font-size: 0.8rem;
  padding: 0.25rem 0.5rem;
}
.outline-item {
  text-align: left;
  background: none;
  border: none;
  border-radius: 5px;
  padding: 0.3rem 0.5rem;
  font-size: 0.82rem;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  color: var(--text-dim);
}
.outline-item:hover {
  background: var(--chrome-raised);
  border: none;
  color: var(--text);
}
.scroller {
  flex: 1;
  overflow-y: auto;
  padding: 2.5rem 1.5rem;
}
.empty {
  height: 100%;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 1rem;
  color: var(--text-dim);
}

/* ---- the Typora-style paper canvas ---- */
.md-page {
  max-width: 740px;
  margin: 0 auto;
  background: #fbfaf7;
  color: #2a2a28;
  border-radius: 10px;
  box-shadow: 0 8px 30px rgba(0, 0, 0, 0.35);
  padding: 4rem 4.5rem 5rem;
}

.link-card input {
  width: 100%;
}
</style>

<style>
/* Unscoped: targets ProseMirror-generated DOM inside .md-page */
.md-prose {
  font-family: var(--serif);
  font-size: 1.05rem;
  line-height: 1.75;
  outline: none;
  min-height: 60vh;
}
.md-prose p.is-editor-empty:first-child::before {
  content: attr(data-placeholder);
  float: left;
  color: #b8b5aa;
  pointer-events: none;
  height: 0;
}
.md-prose h1,
.md-prose h2,
.md-prose h3 {
  font-family: var(--sans);
  color: var(--accent-ink);
  line-height: 1.3;
  margin: 1.6em 0 0.5em;
}
.md-prose h1 {
  font-size: 2em;
  border-bottom: 1px solid #e5e2d8;
  padding-bottom: 0.3em;
}
.md-prose h2 {
  font-size: 1.5em;
}
.md-prose h3 {
  font-size: 1.2em;
}
.md-prose p {
  margin: 0.9em 0;
}
.md-prose ul,
.md-prose ol {
  padding-left: 1.4em;
}
.md-prose li {
  margin: 0.3em 0;
}
.md-prose ul[data-type="taskList"] {
  list-style: none;
  padding-left: 0.2em;
}
.md-prose ul[data-type="taskList"] li {
  display: flex;
  align-items: flex-start;
  gap: 0.5em;
}
.md-prose ul[data-type="taskList"] li > label {
  margin-top: 0.35em;
}
.md-prose blockquote {
  margin: 1em 0;
  padding: 0.2em 1em;
  border-left: 3px solid var(--accent);
  color: #5a574c;
}
.md-prose code {
  font-family: var(--mono);
  font-size: 0.9em;
  background: #efece2;
  border-radius: 4px;
  padding: 0.1em 0.35em;
}
.md-prose pre {
  font-family: var(--mono);
  font-size: 0.85em;
  background: #2a2a28;
  color: #e8e6dd;
  border-radius: 8px;
  padding: 1em 1.2em;
  overflow-x: auto;
}
.md-prose pre code {
  background: none;
  padding: 0;
  color: inherit;
}
.md-prose table {
  border-collapse: collapse;
  width: 100%;
  margin: 1em 0;
}
.md-prose th,
.md-prose td {
  border: 1px solid #ddd9cc;
  padding: 0.4em 0.7em;
}
.md-prose th {
  background: #f0eee5;
  font-weight: 600;
}
.md-prose img {
  max-width: 100%;
  border-radius: 6px;
}
.md-prose hr {
  border: none;
  border-top: 1px solid #e5e2d8;
  margin: 2em 0;
}
</style>
