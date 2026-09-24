<script lang="ts">
  import '@awesome.me/webawesome/dist/components/callout/callout.js';
  import { activeMainTab, storageSecurityStore } from "../lib/stores";
  import { exportFullBundle, importAppConfigFromJsonText, replaceAppConfig, resetAppConfigToDefaults } from "../lib/import-export";
  import JsonPreviewDialog from "./JsonPreviewDialog.svelte";
  import { withSelectedFileText } from "../lib/utils";
  import { appShell } from "../lib/app-shell";
  import { browserWindow } from "../lib/browser";
  import { alertDialog, confirmDialog } from "../lib/dialogs";
  import type { StorageSecurityInfo } from "../lib/storage-security";
  import { formatDesktopSecretsSummary } from "../lib/settings";

  let isJsonModalOpen = $state(false);
  let jsonModalConfig = $state<unknown>({});
  const jsonModalVariants = $derived([{ id: "config", label: "App config", value: jsonModalConfig, editable: true }]);
  let importInputEl = $state<HTMLInputElement | null>(null);
  const isDesktop = appShell.isDesktop();
  const storageSecurity = $derived($storageSecurityStore);
  const storageNotice = $derived(getStorageNotice(storageSecurity));

  function openImportPicker() {
    importInputEl?.click();
  }

  async function handleImportSelected(event: Event) {
    try {
      await withSelectedFileText(event, async (text) => {
        const result = await importAppConfigFromJsonText(text);
        await alertDialog(
          `Imported ${result.importedPublishers} publishers and ${result.importedConsumers} consumers.`,
          "Import complete",
        );
        window.location.reload();
      });
    } catch (error) {
      await alertDialog(`Import failed: ${(error as Error).message}`, "Import");
    }
  }

  async function resetConfig() {
    const confirmed = await confirmDialog(
      "Reset publishers and consumers? Existing entries will be removed.",
      "Reset App Config",
    );
    if (!confirmed) return;
    await resetAppConfigToDefaults();
    window.location.reload();
  }

  function openJsonModal() {
    jsonModalConfig = structuredClone(appShell.config());
    isJsonModalOpen = true;
  }

  async function applyJsonConfig(value: Record<string, unknown>) {
    const confirmed = await confirmDialog(
      "Replace the whole app config with the edited version? Unsaved changes elsewhere are discarded.",
      "Apply Config",
    );
    if (!confirmed) return false;
    await replaceAppConfig(value);
    window.location.reload();
  }

  async function checkStoredSecrets() {
    const alert = browserWindow().mqbAlert || alertDialog;
    try {
      const response = await fetch("/desktop-secrets", { cache: "no-store" });
      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || "Failed to inspect stored secrets");
      }

      const summary = await response.json();
      await alert(formatDesktopSecretsSummary(summary), "Stored Secrets");
    } catch (error) {
      await alert(`Failed to inspect stored secrets: ${(error as Error).message}`, "Stored Secrets");
    }
  }

  async function deleteStoredSecrets() {
    try {
      const confirmed = await confirmDialog(
        "Delete all securely stored secrets referenced by the current desktop config?",
        "Delete Stored Secrets",
      );
      if (!confirmed) {
        return;
      }

      const response = await fetch("/desktop-secrets", { method: "DELETE" });
      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || "Failed to delete stored secrets");
      }

      const result = await response.json().catch(() => ({ deleted: 0 }));
      const deleted = Number(result?.deleted || 0);
      await alertDialog(
        deleted > 0
          ? `Deleted ${deleted} stored secret${deleted === 1 ? "" : "s"}.`
          : "No stored secrets were found for the current desktop config.",
        "Stored Secrets",
      );
    } catch (error) {
      await alertDialog(`Failed to delete stored secrets: ${(error as Error).message}`, "Stored Secrets");
    }
  }

  function getStorageNotice(info: StorageSecurityInfo) {
    if (info.messagesEncrypted && info.messagesPersistent) {
      return "Cached message history is encrypted and restored after restart on this machine.";
    }
    if (info.messagesEncrypted) {
      return "Cached message history is encrypted for this session and cleared after restart.";
    }
    if (info.reason === "key-store-unavailable") {
      return "No OS key store is available, so persistent encrypted storage is not available on this machine.";
    }
    return "Cached message history is stored on disk without encryption in this mode.";
  }
</script>

<div class:active={$activeMainTab === "config"} class="tab-content-panel" id="tab-config">
  <div id="form-tab-wrapper">
    <div
      class="settings-security-banner"
      id="settings-security-banner"
      data-test-id="js-storage-security-note"
    >
      {storageNotice}
    </div>
    <div
      id="form-actions"
      class="section-toolbar editor-action-bar editor-action-bar--config editor-action-bar--compact settings-action-bar-initial"
    >
      <div class="form-actions-row section-actions">
        <div class="section-actions-right">
          <div class="editor-action-cluster">
            <button
              class="wa-native-button wa-native-button--neutral"
              type="button"
              title="Export app config + env vars"
              onclick={exportFullBundle}>Export</button
            >
            <button
              class="wa-native-button wa-native-button--neutral"
              type="button"
              title="Import app config and merge data"
              onclick={openImportPicker}>Import</button
            >
            <button
              class="wa-native-button wa-native-button--danger"
              type="button"
              title="Reset publishers and consumers"
              onclick={resetConfig}>Reset</button
            >
            <input bind:this={importInputEl} type="file" accept=".json,application/json" class="hidden-file-input" onchange={handleImportSelected} />
            <button
              class="wa-native-button wa-native-button--neutral"
              id="js-show-json"
              type="button"
              title="Show current configuration as JSON"
              onclick={openJsonModal}>{`{?} JSON`}</button
            >
            {#if isDesktop}
              <button
                class="wa-native-button wa-native-button--neutral"
                id="js-check-desktop-secrets"
                type="button"
                title="Inspect securely stored secrets referenced by this config"
                onclick={checkStoredSecrets}>Check Stored Secrets</button
              >
              <button
                class="wa-native-button wa-native-button--danger"
                id="js-delete-desktop-secrets"
                type="button"
                title="Delete securely stored secrets referenced by this config"
                onclick={deleteStoredSecrets}>Delete Stored Secrets</button
              >
            {/if}
          </div>
        </div>
      </div>
    </div> 
  </div>
  <div class="form-scroll-wrapper">
    <div id="form-container" class="field-grid"></div>
  </div>
</div>

<JsonPreviewDialog
  open={isJsonModalOpen}
  title="Current Configuration"
  variants={jsonModalVariants}
  onClose={() => (isJsonModalOpen = false)}
  onApply={(_variantId, value) => applyJsonConfig(value)}
/>

<style>
  .settings-security-banner {
    color: var(--text-secondary);
    font-size: 12px;
    line-height: 1.45;
    padding: 0.5rem 0.75rem 0.75rem;
  }
  #form-tab-wrapper {
    position: relative;
  }

  .settings-action-bar-initial,
  .hidden-file-input {
    display: none;
  }
</style>
