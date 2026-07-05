<script setup lang="ts">
import { onMounted, ref } from "vue";
import { api, type LicenseStatus } from "./lib/api";
import LicenseGate from "./components/LicenseGate.vue";
import PdfViewer from "./components/PdfViewer.vue";

const status = ref<LicenseStatus | null>(null);

onMounted(async () => {
  status.value = await api.checkLicense();
});
</script>

<template>
  <div class="app">
    <template v-if="status">
      <PdfViewer v-if="status.licensed" />
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
