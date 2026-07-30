<script setup lang="ts">
import { ref } from "vue";
import { open } from "@tauri-apps/plugin-dialog";
import { api, type LicenseStatus } from "../lib/api";

const emit = defineEmits<{ activated: [status: LicenseStatus] }>();
const error = ref<string | null>(null);
const busy = ref(false);

async function importLease() {
  error.value = null;
  const path = await open({
    title: "Import PDF-EZ license",
    filters: [{ name: "PDF-EZ license", extensions: ["lease"] }],
    multiple: false,
  });
  if (typeof path !== "string") return;
  busy.value = true;
  try {
    const status = await api.activateLicenseFile(path);
    emit("activated", status);
  } catch (e) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <div class="gate">
    <div class="card">
      <p class="wordmark">PD<span>-EZ</span></p>
      <h1>Pay once. Own it forever.</h1>
      <p class="pitch">
        A full PDF editor that runs entirely on your machine. No subscription,
        no account, no cloud — your documents never leave this computer.
        The license is checked once at startup, offline, for life.
      </p>
      <div class="actions">
        <button class="primary" disabled title="Checkout launches with v1.0">
          Buy PDF-EZ — $4.99
        </button>
        <button :disabled="busy" @click="importLease">Import license file…</button>
      </div>
      <p v-if="error" class="error">{{ error }}</p>
    </div>
  </div>
</template>

<style scoped>
.gate {
  height: 100%;
  display: grid;
  place-items: center;
  background: var(--well);
}
.card {
  max-width: 30rem;
  padding: 3rem 3.25rem;
  background: var(--chrome);
  border: 1px solid var(--chrome-line);
  border-radius: 12px;
  text-align: center;
}
.wordmark {
  font-family: var(--mono);
  font-size: 0.95rem;
  letter-spacing: 0.12em;
  text-transform: uppercase;
  color: var(--text-dim);
  margin: 0 0 1.25rem;
}
.wordmark span { color: var(--accent); }
h1 {
  font-family: var(--serif);
  font-size: 1.7rem;
  font-weight: 600;
  margin: 0 0 0.75rem;
  text-wrap: balance;
}
.pitch {
  color: var(--text-dim);
  font-size: 0.92rem;
  line-height: 1.55;
  margin: 0 0 1.75rem;
}
.actions {
  display: flex;
  gap: 0.6rem;
  justify-content: center;
  flex-wrap: wrap;
}
.error {
  margin: 1.25rem 0 0;
  color: var(--danger);
  font-size: 0.85rem;
}
</style>
