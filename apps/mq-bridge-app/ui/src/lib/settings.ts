interface DesktopSecretEntry {
  key?: string;
  extracted?: boolean;
  stored?: boolean;
  error?: string;
}
import { cloneSectionState } from "./dirty-state";
import { appShell, getAppState, workspaceRuntime } from "./app-shell";
import { initSchemaForm } from "./forms/form-cache";
import { availableStorageModeValues, type StorageModeValue, type StorageSecurityInfo } from "./storage-security";
import { ensureWorkspaceCollections } from "./workspace-config";

interface DesktopSecretSummary {
  routes?: Record<string, DesktopSecretEntry[]>;
  consumers?: Record<string, DesktopSecretEntry[]>;
  publishers?: Record<string, DesktopSecretEntry[]>;
}

const SETTINGS_KEYS = [
  "default_tab",
  "log_level",
  "logger",
  "ui_addr",
  "metrics_addr",
  "config_security",
  "env_vars",
] as const;

type SettingsKey = (typeof SETTINGS_KEYS)[number];
const STORAGE_MODE_VALUES = new Set([
  "unencrypted",
  "balanced",
  "env_temporary_messages",
  "temporary_messages",
  "sensitive",
  "durable",
]);

type StorageModeUiOption = {
  value: StorageModeValue;
  title: string;
  detail: string;
};

const STORAGE_MODE_UI: Record<StorageModeValue, { cli?: StorageModeUiOption; desktop?: StorageModeUiOption }> = {
  unencrypted: {
    cli: {
      value: "unencrypted",
      title: "Unencrypted",
      detail: "Plain config and plain cached message history on disk.",
    },
    desktop: {
      value: "unencrypted",
      title: "Unencrypted",
      detail: "Plain config and plain cached message history on disk.",
    },
  },
  balanced: {
    cli: {
      value: "balanced",
      title: "Env secrets",
      detail: "Plain config with secrets extracted to environment placeholders; message history stays plain.",
    },
    desktop: {
      value: "balanced",
      title: "Keychain secrets",
      detail: "Plain config, secrets stored in the OS key store, plain message history.",
    },
  },
  env_temporary_messages: {
    cli: {
      value: "env_temporary_messages",
      title: "Env secrets + temporary encrypted messages",
      detail: "Plain config with env placeholders and encrypted message history cleared after restart.",
    },
  },
  temporary_messages: {
    desktop: {
      value: "temporary_messages",
      title: "Temporary encrypted messages",
      detail: "Plain config with encrypted message history that is cleared after restart.",
    },
  },
  sensitive: {
    desktop: {
      value: "sensitive",
      title: "Encrypted config + temporary encrypted messages",
      detail: "Encrypted config with temporary encrypted message history cleared after restart.",
    },
  },
  durable: {
    desktop: {
      value: "durable",
      title: "Encrypted config + persistent encrypted messages",
      detail: "Encrypted config with persistent encrypted message history restored after restart.",
    },
  },
};

function cloneSchema<T>(schema: T): T {
  return JSON.parse(JSON.stringify(schema)) as T;
}

function filterObjectKeys<T extends Record<string, unknown>>(value: T, keys: readonly SettingsKey[]) {
  const result: Record<string, unknown> = {};
  for (const key of keys) {
    if (Object.prototype.hasOwnProperty.call(value, key)) {
      result[key] = value[key];
    }
  }
  return result;
}

function applyStorageModeDescriptions(
  properties: Record<string, unknown>,
  storageInfo: StorageSecurityInfo | undefined,
  currentConfig?: Record<string, unknown>,
) {
  const configSecurity = properties.config_security as Record<string, unknown> | undefined;
  const nestedProperties = configSecurity?.properties as Record<string, unknown> | undefined;
  const modeProperty = nestedProperties?.mode as Record<string, unknown> | undefined;
  if (!configSecurity || !modeProperty || !storageInfo) {
    return;
  }

  configSecurity.description = "";
  modeProperty.description = formatStorageModeDescription(
    storageInfo,
    typeof (currentConfig as any)?.config_security?.mode === "string"
      ? (currentConfig as any).config_security.mode
      : undefined,
  );
}

function getStorageModeUiOptions(
  storageInfo: StorageSecurityInfo,
): StorageModeUiOption[] {
  const target = storageInfo.target === "desktop" ? "desktop" : "cli";
  return availableStorageModeValues(storageInfo)
    .map((value) => STORAGE_MODE_UI[value][target])
    .filter((option): option is StorageModeUiOption => Boolean(option));
}

function formatStorageModeDescription(storageInfo: StorageSecurityInfo, currentMode?: string) {
  const options = getStorageModeUiOptions(storageInfo);
  const parts = options.map((option) => `${option.title}: ${option.detail}`);
  if (currentMode && STORAGE_MODE_VALUES.has(currentMode as StorageModeValue) && !options.some((option) => option.value === currentMode)) {
    parts.push(`${currentMode} (Current, unsupported here): Unavailable.`);
  }

  if (storageInfo.target === "desktop" && storageInfo.keyStoreAvailable !== true) {
    parts.push("Only the modes shown here are available because no OS key store is currently usable.");
  }

  return parts.join(" ");
}

export function buildSettingsSchema(
  schema: Record<string, unknown>,
  storageInfo?: StorageSecurityInfo,
  currentConfig?: Record<string, unknown>,
) {
  const cloned = cloneSchema(schema);
  const properties = filterObjectKeys(
    ((cloned?.properties as Record<string, unknown> | undefined) || {}) as Record<string, unknown>,
    SETTINGS_KEYS,
  );
  applyStorageModeDescriptions(
    properties,
    storageInfo,
    currentConfig,
  );
  return {
    ...cloned,
    properties,
    required: Array.isArray(cloned?.required)
      ? cloned.required.filter((key) => typeof key === "string" && Object.prototype.hasOwnProperty.call(properties, key))
      : [],
  };
}

export function extractSettingsConfig(config: Record<string, unknown>) {
  const normalized = ensureWorkspaceCollections({ ...(config as Record<string, unknown>) });
  return filterObjectKeys(normalized, SETTINGS_KEYS);
}

export function mergeSettingsConfig(target: Record<string, unknown>, settingsConfig: Record<string, unknown>) {
  for (const key of SETTINGS_KEYS) {
    if (Object.prototype.hasOwnProperty.call(settingsConfig, key)) {
      target[key] = settingsConfig[key];
    }
  }
}

function pruneStorageModeOptions(
  container: HTMLElement,
  storageInfo: StorageSecurityInfo | undefined,
) {
  if (!storageInfo) {
    return;
  }

  const allowed = new Set(availableStorageModeValues(storageInfo));

  const candidates = Array.from(container.querySelectorAll("select")) as HTMLSelectElement[];
  const select = candidates.find((candidate) => {
    const optionValues = Array.from(candidate.options).map((option) => option.value);
    const storageMatches = optionValues.filter((value) => STORAGE_MODE_VALUES.has(value)).length;
    return storageMatches >= 3;
  });
  if (!select) {
    return;
  }

  for (const option of Array.from(select.options)) {
    if (STORAGE_MODE_VALUES.has(option.value) && !allowed.has(option.value as any)) {
      option.remove();
    }
  }
}

export function formatDesktopSecretsSummary(summary: DesktopSecretSummary): string {
  const groups: Array<[string, Record<string, DesktopSecretEntry[]>]> = [
    ["Routes", summary.routes || {}],
    ["Consumers", summary.consumers || {}],
    ["Publishers", summary.publishers || {}],
  ];

  const lines: string[] = [];
  for (const [label, entries] of groups) {
    const names = Object.keys(entries).sort();
    if (names.length === 0) {
      continue;
    }

    lines.push(`${label}:`);
    for (const name of names) {
      const items = entries[name] || [];
      const extracted = items.filter((item) => item.extracted).length;
      const stored = items.filter((item) => item.stored).length;
      const total = items.length;

      lines.push(`- ${name}: ${extracted}/${total} extracted, ${stored}/${total} stored`);

      const errors = items
        .filter((item) => item.error)
        .map((item) => `${item.key}: ${item.error}`);
      errors.forEach((message) => lines.push(`  ${message}`));
    }
  }

  return lines.length > 0
    ? lines.join("\n")
    : "The current desktop config does not reference any extracted secrets.";
}

export async function initSettings(config: Record<string, unknown>, schema: Record<string, unknown>) {
  const lib = appShell.forms();
  const container = document.getElementById("form-container");
  if (!container) {
    return;
  }

  const state = getAppState();
  const settingsSchema = buildSettingsSchema(schema, state.storage_security, config);
  let settingsConfig = extractSettingsConfig(config);
  state.saved_sections.config = cloneSectionState(settingsConfig);

  workspaceRuntime.registerDirtySection("config", {
    buttonId: "workspace-save-button",
    getValue: () => settingsConfig,
  });
  workspaceRuntime.registerBeforeWorkspaceSave("config", () => {
    mergeSettingsConfig(appShell.config<Record<string, unknown>>(), settingsConfig);
  });
  workspaceRuntime.registerAfterWorkspaceSave("config", (savedConfig) => {
    settingsConfig = extractSettingsConfig(savedConfig);
  });

  state.form_mode = "settings";
  (window as any)._mqb_form_mode = "settings";
  await initSchemaForm(lib, container, settingsSchema, settingsConfig);
  pruneStorageModeOptions(
    container,
    state.storage_security,
  );

  const formActions = document.getElementById("form-actions");
  if (formActions) {
    formActions.style.display = "flex";
  }

  const scheduleDirtyRefresh = () => setTimeout(() => workspaceRuntime.refreshDirtySection("config"), 0);
  container.oninput = scheduleDirtyRefresh;
  container.onchange = scheduleDirtyRefresh;
}
