<script setup lang="ts">
import { onMounted, ref } from "vue";
import { api, type LicenseStatus } from "./lib/api";
import LicenseGate from "./components/LicenseGate.vue";
import PdfViewer from "./components/PdfViewer.vue";
import MarkdownEditor from "./components/MarkdownEditor.vue";

const status = ref<LicenseStatus | null>(null);
const docKind = ref<"pdf" | "markdown">("pdf");
const openPath = ref<string | undefined>(undefined);
const viewKey = ref(0);

function kindOf(path: string): "pdf" | "markdown" {
  return /\.(md|markdown)$/i.test(path) ? "markdown" : "pdf";
}

function onSwitchKind(path: string) {
  docKind.value = kindOf(path);
  openPath.value = path;
  viewKey.value++;
}

onMounted(async () => {
  status.value = await api.checkLicense();
  const launch = await api.getLaunchFile();
  if (launch) {
    docKind.value = kindOf(launch);
    openPath.value = launch;
  }
});
</script>

<template>
  <div class="app">
    <template v-if="status">
      <template v-if="status.licensed">
        <PdfViewer
          v-if="docKind === 'pdf'"
          :key="'pdf-' + viewKey"
          :open-path="openPath"
          @switch-kind="onSwitchKind"
        />
        <MarkdownEditor
          v-else
          :key="'md-' + viewKey"
          :open-path="openPath"
          @switch-kind="onSwitchKind"
        />
      </template>
      <LicenseGate v-else @activated="(s) => (status = s)" />
      <span v-if="status.licensed && status.dev_build && !status.email" class="devbadge">DEV</span>
    </template>
  </div>
</template>

<style scoped>
.app {
  height: 100%;
  position: relative;
}
.devbadge {
  position: absolute;
  top: 0.6rem;
  right: 0.75rem;
  font-family: var(--mono);
  font-size: 0.65rem;
  letter-spacing: 0.1em;
  color: var(--well);
  background: var(--accent);
  border-radius: 4px;
  padding: 0.1rem 0.4rem;
  pointer-events: none;
}
</style>
