import { tick } from "svelte";
import {
  activeMainTab,
  runtimeStatusStore,
  storageSecurityStore,
  workspaceDirtyStore,
  workspaceSavingStore,
} from "./lib/stores";
import { setPeerStatus } from "./lib/state/peer-status.svelte";
import { createDirtyTracker, cloneSectionState, isDirty, serializeSectionState } from "./lib/dirty-state";
import {
  saveWholeConfig,
  saveConfigSection as persistConfigSection,
  fetchConfigFromServer,
  fetchConfigRecoveryFromServer,
  fetchStorageSecurityFromServer,
  postResetConfigRecovery,
} from "./lib/config-api";
import { initConsumers, restoreConsumerStateFromView } from "./lib/consumers-view";
import { initPublishers, restorePublisherStateFromView } from "./lib/publishers-view";
import { EMPTY_RUNTIME_STATUS, createRuntimeStatusPoller, type MainTab } from "./lib/runtime-status";
import { nextHashForTab, pickDefaultTab, resolveTabFromHash } from "./lib/routing";
import { registerConfigSchema } from "./lib/save-error";
import { extractSettingsConfig, initSettings } from "./lib/settings";
import {
  appShell,
  configureAppShell,
  getAppState,
  resetAppState,
  workspaceRuntime,
} from "./lib/app-shell";
import {
  clearLegacyPendingRestoreGlobals,
  currentHash,
  mqbDialogs,
  onHashChange,
  replaceHash,
} from "./lib/runtime-window";
import { EMPTY_STORAGE_SECURITY, normalizeStorageSecurityInfo } from "./lib/storage-security";
import { getStoredJson } from "./lib/encrypted-json-storage";
import { browserWindow } from "./lib/browser";
import { getAvailableFeatures } from "./lib/feature-detection";
import { registerEndpointKindsFromSchema } from "./lib/endpoint-metadata";

type ConfigRecoveryStatus = {
  mode?: string;
  reason?: string;
  message?: string;
  detail?: string;
} | null;

function replaceLiveConfig<T extends Record<string, unknown>>(appConfig: T, refreshedConfig: T | null | undefined) {
  if (!refreshedConfig) return;
  Object.keys(appConfig).forEach((key) => {
    delete appConfig[key];
  });
  Object.assign(appConfig, refreshedConfig);
}

async function maybeHandleConfigRecovery(fetchImpl: typeof fetch): Promise<void> {
  const recovery = await fetchConfigRecoveryFromServer<ConfigRecoveryStatus>(fetchImpl).catch(() => null);
  if (!recovery?.message) return;

  const choice = await mqbDialogs.choose(
    `${recovery.message}${recovery.detail ? `\n\n${recovery.detail}` : ""}`,
    "Encrypted Config Recovery",
    {
      confirmLabel: "Continue",
      cancelLabel: "Close",
      choices: [
        {
          value: "continue",
          label: "Continue",
          description: "Open the app with a temporary empty config for now.",
        },
        {
          value: "reset",
          label: "Reset Config",
          description: "Back up the unreadable config and replace it with a fresh default config.",
        },
      ],
    },
  );

  if (choice === "reset") {
    const result = await postResetConfigRecovery<{ backup_path?: string }>(fetchImpl);
    await mqbDialogs.alert(
      result?.backup_path
        ? `The unreadable config was backed up to:\n${result.backup_path}`
        : "The unreadable config was reset.",
      "Encrypted Config Reset",
    );
  }
}

function setActiveTab(name: MainTab) {
  activeMainTab.set(name);
  getAppState().active_tab = name;
}

function rememberedIndexForTab(name: MainTab): number | undefined {
  const state = getAppState();
  if (name === "consumers") return state.last_consumer_idx ?? 0;
  if (name === "publishers") return state.last_publisher_idx ?? 0;
  return undefined;
}

function rememberSelectionFromHash(hash: string) {
  const state = getAppState();
  const publisherMatch = hash.match(/^#publishers:(\d+)$/);
  const consumerMatch = hash.match(/^#consumers:(\d+)$/);
  if (publisherMatch) state.last_publisher_idx = parseInt(publisherMatch[1], 10);
  if (consumerMatch) state.last_consumer_idx = parseInt(consumerMatch[1], 10);
}

async function initTabIfNeeded(name: MainTab) {
  const state = getAppState();
  if (name === "consumers" && !state.consumers_initialized) {
    await appShell.init.consumers(appShell.config(), appShell.schema());
    state.consumers_initialized = true;
  }
  if (name === "publishers" && !state.publishers_initialized) {
    await appShell.init.publishers(appShell.config(), appShell.schema());
    state.publishers_initialized = true;
  }
  if (name === "config" && !state.config_initialized) {
    await initSettings(appShell.config(), appShell.schema());
    state.config_initialized = true;
  }
}

async function restoreTabState(name: MainTab) {
  const state = getAppState();
  if (name === "consumers") {
    const pending = state.pending_consumer_restore || null;
    state.pending_consumer_restore = null;
    clearLegacyPendingRestoreGlobals();
    const match = currentHash().match(/^#consumers:(\d+)$/);
    const idx = pending?.idx ?? (match ? parseInt(match[1], 10) : (state.last_consumer_idx ?? 0));
    state.last_consumer_idx = idx;
    await restoreConsumerStateFromView(idx, { tab: pending?.tab ?? state.last_consumer_tab });
    return;
  }

  if (name === "publishers") {
    const pending = state.pending_publisher_restore || null;
    state.pending_publisher_restore = null;
    clearLegacyPendingRestoreGlobals();
    const match = currentHash().match(/^#publishers:(\d+)$/);
    const idx = pending?.idx ?? (match ? parseInt(match[1], 10) : (state.last_publisher_idx ?? 0));
    state.last_publisher_idx = idx;
    await restorePublisherStateFromView(idx, { tab: pending?.tab ?? state.last_publisher_tab });
  }
}

export async function switchMain(name: MainTab) {
  rememberSelectionFromHash(currentHash());
  setActiveTab(name);
  await tick();
  await initTabIfNeeded(name);
  await tick();
  await restoreTabState(name);
  replaceHash(nextHashForTab(currentHash(), name, rememberedIndexForTab(name)));
}

function syncSaveButtonLabel(button: HTMLElement | null) {
  if (!button) return;
  const baseLabel = button.dataset.baseLabel || button.textContent?.trim() || "Save";
  button.dataset.baseLabel = baseLabel;
  if (button.dataset.saving === "true") return;
  const dirty = button.dataset.dirty === "true";
  button.classList.toggle("is-dirty", dirty);
}

function computeWorkspaceDirty(): boolean {
  return Object.values(getAppState().dirty_sections || {}).some((tracker) => isDirty(tracker));
}

function renderWorkspaceDirty() {
  const dirty = computeWorkspaceDirty();
  workspaceDirtyStore.set(dirty);
  const saveButton = document.getElementById("workspace-save-button");
  if (saveButton) {
    saveButton.dataset.dirty = dirty ? "true" : "false";
    syncSaveButtonLabel(saveButton);
  }
  return dirty;
}

function refreshDirtySection(sectionName: string): boolean {
  const tracker = getAppState().dirty_sections?.[sectionName];
  if (!tracker) return false;
  const dirty = isDirty(tracker);
  const button = document.getElementById(tracker.buttonId);
  if (button) {
    button.dataset.dirty = dirty ? "true" : "false";
    syncSaveButtonLabel(button);
  }
  renderWorkspaceDirty();
  return dirty;
}

function renderRuntimeStatus(status?: typeof EMPTY_RUNTIME_STATUS) {
  runtimeStatusStore.set(status || getAppState().runtime_status || EMPTY_RUNTIME_STATUS);
}

function renderStorageSecurity() {
  storageSecurityStore.set(getAppState().storage_security || { ...EMPTY_STORAGE_SECURITY });
}

async function replaceConfigFromSave(appConfig: Record<string, unknown>) {
  const refreshedConfig = await saveWholeConfig(fetch, appConfig);
  const refreshedStorageSecurity = await fetchStorageSecurityFromServer(fetch).catch(() => null);
  replaceLiveConfig(appConfig, refreshedConfig);
  if (refreshedStorageSecurity) {
    const normalizedStorageSecurity = normalizeStorageSecurityInfo(refreshedStorageSecurity);
    const state = getAppState();
    state.storage_security = normalizedStorageSecurity;
    browserWindow()._mqb_storage_security = normalizedStorageSecurity;
    renderStorageSecurity();
  }
  return refreshedConfig;
}

async function runSaveAction<T>(
  button: HTMLElement | null,
  silent: boolean,
  action: () => Promise<T>,
  options: { trackWorkspaceSaving?: boolean } = {},
): Promise<T | null> {
  try {
    if (options.trackWorkspaceSaving) workspaceSavingStore.set(true);
    return await action();
  } catch (error) {
    if (!silent) await mqbDialogs.alert(`Error saving: ${(error as Error).message}`);
    return null;
  } finally {
    if (options.trackWorkspaceSaving) workspaceSavingStore.set(false);
    if (button) {
      button.dataset.saving = "false";
      (button as HTMLButtonElement).disabled = false;
      syncSaveButtonLabel(button);
    }
  }
}

const runtimeStatusPoller = createRuntimeStatusPoller({
  onStatus: (status) => {
    getAppState().runtime_status = status;
    browserWindow()._mqb_runtime_status = status;
    renderRuntimeStatus(status);
    browserWindow().renderConsumersRuntimeStatus?.();
    browserWindow().renderRoutesRuntimeMetrics?.();
  },
  onPeerStatus: (status) => {
    setPeerStatus(status);
  },
});

async function reinitializeWorkspaceViews() {
  const state = getAppState();
  if (state.publishers_initialized) await initPublishers(appShell.config(), appShell.schema());
  if (state.consumers_initialized) await initConsumers(appShell.config(), appShell.schema());
  if (state.config_initialized) await initSettings(appShell.config(), appShell.schema());
}

function registerDirtySection(sectionName: string, options: { buttonId: string; getValue: () => unknown }) {
  if (!sectionName || !options?.buttonId || typeof options.getValue !== "function") return;
  const state = getAppState();
  const [key, tracker] = createDirtyTracker(
    sectionName,
    options.buttonId,
    options.getValue,
    state.dirty_sections[sectionName],
    state.saved_sections,
  );
  state.dirty_sections[key] = tracker;
  refreshDirtySection(sectionName);
}

function registerBeforeWorkspaceSave(key: string, callback: () => void | Promise<void>) {
  if (!key || typeof callback !== "function") return;
  getAppState().before_workspace_save_hooks[key] = callback;
}

function registerAfterWorkspaceSave(
  key: string,
  callback: (savedConfig: Record<string, unknown>) => void | Promise<void>,
) {
  if (!key || typeof callback !== "function") return;
  getAppState().after_workspace_save_hooks[key] = callback;
}

function markSectionSaved(sectionName: string, savedValue?: unknown) {
  const state = getAppState();
  const tracker = state.dirty_sections?.[sectionName];
  const nextSavedValue = savedValue === undefined ? tracker?.getValue?.() : savedValue;
  state.saved_sections[sectionName] = cloneSectionState(nextSavedValue);
  if (!tracker) return;
  tracker.baseline = serializeSectionState(state.saved_sections[sectionName]);
  refreshDirtySection(sectionName);
}

function initializeAppShell() {
  resetAppState();
  renderStorageSecurity();
  workspaceDirtyStore.set(false);
  workspaceSavingStore.set(false);
  configureAppShell({
    initConsumers,
    initPublishers,
    switchMain,
    syncSaveButtonLabel,
    refreshDirtySection,
    pollRuntimeStatus: () => runtimeStatusPoller.poll(),
    fetchConfigFromServer: <T>() => fetchConfigFromServer<T>(fetch),
    registerDirtySection,
    registerBeforeWorkspaceSave,
    registerAfterWorkspaceSave,
    markSectionSaved,
    saveWorkspace: async (
      silent = false,
      button: (HTMLElement & { loading?: boolean }) | null = null,
    ) => {
      if (button) {
        button.dataset.saving = "true";
        (button as HTMLButtonElement).disabled = true;
      }
      return runSaveAction(button, silent, async () => {
        const state = getAppState();
        for (const hook of Object.values(state.before_workspace_save_hooks)) {
          await hook();
        }
        const appConfig = appShell.config<Record<string, unknown>>();
        const refreshedConfig = await replaceConfigFromSave(appConfig);
        for (const hook of Object.values(state.after_workspace_save_hooks)) {
          await hook(appConfig);
        }
        await reinitializeWorkspaceViews();
        markSectionSaved("publishers", appConfig.publishers ?? []);
        markSectionSaved("consumers", appConfig.consumers ?? []);
        markSectionSaved("config", extractSettingsConfig(appConfig));
        return refreshedConfig;
      }, { trackWorkspaceSaving: true });
    },
    saveConfig: async (
      silent = false,
      button: (HTMLElement & { loading?: boolean }) | null = null,
    ) => {
      if (button) {
        button.dataset.saving = "true";
        (button as HTMLButtonElement).disabled = true;
      }
      return runSaveAction(button, silent, async () => {
        const appConfig = appShell.config<Record<string, unknown>>();
        const refreshedConfig = await replaceConfigFromSave(appConfig);
        markSectionSaved("config", extractSettingsConfig(appConfig));
        return refreshedConfig;
      });
    },
    saveConfigSection: async (
      sectionName: string,
      sectionValue: unknown,
      silent = false,
      button: (HTMLElement & { loading?: boolean }) | null = null,
    ) => {
      if (button) {
        button.dataset.saving = "true";
        (button as HTMLButtonElement).disabled = true;
      }
      return runSaveAction(button, silent, async () => {
        const appConfig = appShell.config<Record<string, unknown>>();
        const refreshedConfig = await persistConfigSection<Record<string, unknown>, string>(
          fetch,
          appConfig,
          sectionName,
          sectionValue,
        );
        appConfig[sectionName] = (refreshedConfig as Record<string, unknown>)[sectionName];
        markSectionSaved(sectionName, (refreshedConfig as Record<string, unknown>)[sectionName]);
        return refreshedConfig;
      });
    },
  });
}

export function saveWorkspace(button?: HTMLElement | null, silent = false) {
  return workspaceRuntime.saveWorkspace(silent, button) ?? Promise.resolve(null);
}

export async function bootstrapApp() {
  initializeAppShell();
  const startupHash = currentHash();
  await maybeHandleConfigRecovery(fetch);
  const [config, schema, storageSecurityRaw, features] = await Promise.all([
    fetchConfigFromServer<Record<string, any>>(fetch),
    fetch("/schema.json").then((response) => response.json()),
    fetchStorageSecurityFromServer(fetch).catch(() => EMPTY_STORAGE_SECURITY),
    getAvailableFeatures(),
  ]);
  const storageSecurity = normalizeStorageSecurityInfo(storageSecurityRaw);
  delete config.routes;
  appShell.setConfig(config);
  appShell.setSchema(schema);
  registerConfigSchema(schema);
  registerEndpointKindsFromSchema(schema);
  const state = getAppState();
  state.storage_security = storageSecurity;
  state.features = features;
  browserWindow()._mqb_storage_security = storageSecurity;
  browserWindow()._mqb_features = features;
  renderStorageSecurity();
  state.storage_cache = {
    publisher_state: await getStoredJson("mqb_publisher_state", {}, storageSecurity),
    publisher_history: await getStoredJson("mqb_publisher_history", {}, storageSecurity),
    consumer_messages: await getStoredJson("mqb_consumer_messages", {}, storageSecurity),
  };
  state.saved_sections = {
    consumers: cloneSectionState(config.consumers),
    publishers: cloneSectionState(config.publishers),
    config: cloneSectionState(config),
  };
  renderWorkspaceDirty();

  onHashChange(() => {
    const targetTab = resolveTabFromHash(currentHash());
    if (targetTab) {
      void switchMain(targetTab);
    }
  });

  renderRuntimeStatus();
  await runtimeStatusPoller.poll();

  const defaultTab = pickDefaultTab(
    startupHash,
    state.runtime_status.active_routes || [],
    config.default_tab,
  );
  if (startupHash) {
    replaceHash(startupHash);
  }
  await switchMain(defaultTab);
  runtimeStatusPoller.stop();
  runtimeStatusPoller.start();
  state.runtime_poll_timer = 1;
  document.documentElement.dataset.mqbReady = "true";
}
