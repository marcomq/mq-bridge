// @vitest-environment jsdom

import { beforeEach, describe, expect, test, vi } from "vitest";
import {
  buildSettingsSchema,
  extractSettingsConfig,
  formatDesktopSecretsSummary,
  initSettings,
} from "../../ui/src/lib/settings";

describe("settings", () => {
  beforeEach(() => {
    document.body.innerHTML = `
      <div id="form-actions" style="display:none;">
      </div>
      <div id="form-container"></div>
    `;

    window.appConfig = { consumers: [], publishers: [], default_tab: "publishers" };
    window.registerDirtySection = vi.fn();
    window.registerBeforeWorkspaceSave = vi.fn();
    window.registerAfterWorkspaceSave = vi.fn();
    window.refreshDirtySection = vi.fn();
    window.markSectionSaved = vi.fn();
    window.saveConfig = vi.fn().mockResolvedValue(true);
    window.mqbAlert = vi.fn().mockResolvedValue(undefined);
    window.mqbConfirm = vi.fn().mockResolvedValue(true);
    window.__MQB_DESKTOP__ = false;
    window._mqb_storage_security = {
      target: "cli",
      encrypted: true,
      persistent: false,
      keySource: "ephemeral-process",
      keyStoreAvailable: false,
      encryptedConfigAvailable: false,
      persistentMessagesAvailable: false,
      configEncrypted: false,
      messagesEncrypted: true,
      messagesPersistent: false,
    };
    window.VanillaSchemaForms = {
      h: (tagName: string, attrs: Record<string, string>, label: string) => {
        const element = document.createElement(tagName);
        Object.entries(attrs).forEach(([key, value]) => element.setAttribute(key, value));
        element.textContent = label;
        return element;
      },
      init: vi.fn().mockImplementation(async (_container: HTMLElement, _schema: unknown, _data: unknown, onChange: (data: unknown) => void) => {
        (window as any).__settingsChange = onChange;
        return {
          setData: (data: Record<string, unknown>) => {
            (window as any).__settingsData = data;
          },
        };
      }),
    } as any;
  });

  test("builds a compact settings schema and config payload", () => {
    const schema = buildSettingsSchema({
      properties: {
        default_tab: { type: "string" },
        log_level: { type: "string" },
        env_vars: { type: "object" },
        publishers: { type: "array" },
        consumers: { type: "array" },
      },
      required: ["default_tab", "publishers"],
    });
    const settings = extractSettingsConfig({
      default_tab: "publishers",
      log_level: "info",
      env_vars: { BASE_URL: "https://example.test" },
      publishers: [{ name: "pub-a" }],
    });

    expect(schema).toEqual({
      properties: {
        default_tab: { type: "string" },
        log_level: { type: "string" },
        env_vars: { type: "object" },
      },
      required: ["default_tab"],
    });
    expect(settings).toEqual({
      default_tab: "publishers",
      log_level: "info",
      env_vars: { BASE_URL: "https://example.test" },
      config_security: { mode: "balanced" },
    });
  });

  test("annotates security mode options with target capabilities", () => {
    const schema = buildSettingsSchema(
      {
        properties: {
          config_security: {
            type: "object",
            properties: {
              mode: {
                type: "string",
                enum: ["unencrypted", "balanced", "env_temporary_messages", "temporary_messages", "sensitive", "durable"],
              },
            },
          },
        },
      },
      window._mqb_storage_security as any,
    );

    expect((schema.properties.config_security as any).description).toBe("");
    expect((schema.properties.config_security as any).properties.mode.description).toContain(
      "Env secrets + temporary encrypted messages",
    );
    expect((schema.properties.config_security as any).properties.mode.description).not.toContain("Sensitive");
  });

  test("describes an unsupported current mode as unavailable", () => {
    const schema = buildSettingsSchema(
      {
        properties: {
          config_security: {
            type: "object",
            properties: {
              mode: {
                type: "string",
                enum: ["unencrypted", "balanced", "env_temporary_messages", "temporary_messages", "sensitive", "durable"],
              },
            },
          },
        },
      },
      {
        target: "cli",
        encrypted: false,
        persistent: true,
        keySource: "env",
        keyStoreAvailable: false,
        encryptedConfigAvailable: false,
        persistentMessagesAvailable: false,
        configEncrypted: false,
        messagesEncrypted: false,
        messagesPersistent: true,
      },
      {
        config_security: { mode: "durable" },
      },
    );

    expect((schema.properties.config_security as any).properties.mode.description).toContain(
      "durable (Current, unsupported here): Unavailable.",
    );
  });

  test("normalizes legacy security settings into config_security", () => {
    const settings = extractSettingsConfig({
      default_tab: "publishers",
      extract_secrets: false,
    });

    expect(settings).toEqual({
      default_tab: "publishers",
      config_security: { mode: "unencrypted" },
      env_vars: {},
    });
  });

  test("formats desktop secret summary by section and name", () => {
    expect(
      formatDesktopSecretsSummary({
        routes: {
          route_b: [
            { key: "token", extracted: true, stored: true },
          ],
          route_a: [
            { key: "user", extracted: true, stored: false, error: "missing in keyring" },
            { key: "pass", extracted: false, stored: false },
          ],
        },
        consumers: {
          consumer_a: [{ key: "auth", extracted: true, stored: true }],
        },
      }),
    ).toBe(
      [
        "Routes:",
        "- route_a: 1/2 extracted, 0/2 stored",
        "  user: missing in keyring",
        "- route_b: 1/1 extracted, 1/1 stored",
        "Consumers:",
        "- consumer_a: 1/1 extracted, 1/1 stored",
      ].join("\n"),
    );
  });

  test("returns empty-summary message when nothing is configured", () => {
    expect(formatDesktopSecretsSummary({})).toBe(
      "The current desktop config does not reference any extracted secrets.",
    );
  });

  test("initializes the settings form and wires save handling", async () => {
    window.appConfig = {
      consumers: [],
      publishers: [{ name: "pub-a" }],
      default_tab: "publishers",
      log_level: "info",
      env_vars: { BASE_URL: "https://example.test" },
      config_security: { mode: "balanced" },
    };

    await initSettings(
      window.appConfig as Record<string, unknown>,
      {
        properties: {
          default_tab: { type: "string" },
          log_level: { type: "string" },
          env_vars: { type: "object" },
          publishers: { type: "array" },
        },
      } as Record<string, unknown>,
    );

    expect(window.registerDirtySection).toHaveBeenCalledWith("config", {
      buttonId: "workspace-save-button",
      getValue: expect.any(Function),
    });
    expect(window.registerBeforeWorkspaceSave).toHaveBeenCalledWith("config", expect.any(Function));
    expect(window.registerAfterWorkspaceSave).toHaveBeenCalledWith("config", expect.any(Function));
    expect(window.VanillaSchemaForms.init).toHaveBeenCalledWith(
      document.getElementById("form-container"),
      {
        properties: {
          default_tab: { type: "string" },
          log_level: { type: "string" },
          env_vars: { type: "object" },
        },
        required: [],
      },
      {},
      expect.any(Function),
    );
    expect((window as any).__settingsData).toEqual({
      default_tab: "publishers",
      log_level: "info",
      env_vars: { BASE_URL: "https://example.test" },
      config_security: { mode: "balanced" },
    });
    expect(document.getElementById("form-actions")?.style.display).toBe("flex");
    expect(document.getElementById("storage-security-note")).toBeNull();
    expect(document.getElementById("storage-mode-note")).toBeNull();

    (window as any).__settingsChange({
      ...structuredClone((window as any).__settingsData),
      log_level: "debug",
      env_vars: { BASE_URL: "https://changed.test", API_TOKEN: "abc" },
      config_security: { mode: "balanced" },
    });
    const beforeSaveHook = (window.registerBeforeWorkspaceSave as any).mock.calls[0][1] as () => void;
    beforeSaveHook();
    expect(window.appConfig.log_level).toBe("debug");
    expect(window.appConfig.env_vars).toEqual({ BASE_URL: "https://changed.test", API_TOKEN: "abc" });
    expect(window.appConfig.config_security).toEqual({ mode: "balanced" });
    expect(window.appConfig.publishers).toEqual([{ name: "pub-a" }]);

    const formContainer = document.getElementById("form-container") as HTMLDivElement;
    formContainer.oninput?.(new InputEvent("input"));
    await new Promise((resolve) => window.setTimeout(resolve, 0));
    expect(window.refreshDirtySection).toHaveBeenCalledWith("config");
  });

  test("prunes rendered storage mode options for cli", async () => {
    window.appConfig = {
      consumers: [],
      publishers: [],
      config_security: { mode: "balanced" },
    };

    window.VanillaSchemaForms.init = vi.fn().mockImplementation(async (container: HTMLElement) => {
      const select = document.createElement("select");
      select.name = "config_security.mode";
      ["unencrypted", "balanced", "env_temporary_messages", "temporary_messages", "sensitive", "durable"].forEach((value) => {
        const option = document.createElement("option");
        option.value = value;
        option.textContent = value;
        select.appendChild(option);
      });
      container.appendChild(select);
      return { setData: vi.fn() };
    });

    await initSettings(
      window.appConfig as Record<string, unknown>,
      {
        properties: {
          config_security: {
            type: "object",
            properties: {
              mode: {
                type: "string",
                enum: ["unencrypted", "balanced", "env_temporary_messages", "temporary_messages", "sensitive", "durable"],
              },
            },
          },
        },
      } as Record<string, unknown>,
    );

    const select = document.querySelector('select[name="config_security.mode"]') as HTMLSelectElement | null;
    expect(select).not.toBeNull();
    expect(Array.from(select?.options || []).map((option) => option.value)).toEqual([
      "unencrypted",
      "balanced",
      "env_temporary_messages",
    ]);
  });

});
