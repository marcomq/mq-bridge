import AdditionalPropertyRow from "./AdditionalPropertyRow.svelte";
import ArrayItemRow from "./ArrayItemRow.svelte";
import BasicAuthEditor from "./BasicAuthEditor.svelte";
import CheckboxField from "./CheckboxField.svelte";
import CollapsibleFields from "./CollapsibleFields.svelte";
import Endpoint from "./Endpoint.svelte";
import FormField from "./FormField.svelte";
import GenericAdditionalPropertyRow from "./GenericAdditionalPropertyRow.svelte";
import HeadersEditor from "./HeadersEditor.svelte";
import OptionalSectionField from "./OptionalSectionField.svelte";
import PasswordField from "./PasswordField.svelte";
import ScalarEndpointInput from "./ScalarEndpointInput.svelte";
import { renderSvelteNode } from "./render-svelte";
import { appShell } from "../app-shell";
import { pickFilePath } from "../desktop-file-dialog";
import { BASIC_ENDPOINT_FIELDS, collectPublisherIdSuggestions } from "../endpoint-metadata";
import {
  createTypeSelectArrayRenderer,
  domRenderer as baseDomRenderer,
  formatWebAwesomeLabel,
  hydrateNodeWithData,
  renderNode,
  renderObject,
  renderProperties,
  rendererConfig as baseRendererConfig,
  setConfig,
  setCustomRenderers,
  setI18n,
  toStorePath,
} from "vanilla-schema-forms";
import * as VanillaSchemaForms from "vanilla-schema-forms";


type SchemaNode = Record<string, any>;
type RendererContext = Record<string, any>;
type CustomRenderer = {
  render: (...args: any[]) => Node;
};
type RendererConfig = {
  classes: Record<string, string>;
  triggers: Record<string, string>;
};
type DomRenderer = Record<string, any> & {
  renderAdditionalProperties: (...args: any[]) => HTMLElement;
  renderBoolean?: (...args: any[]) => HTMLElement;
  renderFieldWrapper: (...args: any[]) => HTMLElement;
  renderHeadlessObject: (...args: any[]) => HTMLElement;
  renderObject: (...args: any[]) => HTMLElement;
  renderOneOf: (...args: any[]) => Node | null;
};

const rendererConfig = baseRendererConfig as RendererConfig;
const domRenderer = baseDomRenderer as DomRenderer;

function withRendererDefaults(context: RendererContext): RendererContext {
  const config = context.config || {};
  const storeData = context.store?.data;
  return {
    ...context,
    data: context.data ?? storeData ?? {},
    onChange: context.onChange || ((nextData: any) => {
      if (!context.store || !context.store.data || !nextData || typeof nextData !== "object") {
        return;
      }
      const target = context.store.data;
      Object.keys(target).forEach((key) => {
        delete target[key];
      });
      Object.assign(target, structuredClone(nextData));
    }),
    nodeRegistry: context.nodeRegistry || new Map(),
    dataPathRegistry: context.dataPathRegistry || new Map(),
    elementIdToDataPath: context.elementIdToDataPath || new Map(),
    customRenderers: context.customRenderers || {},
    rootNode: context.rootNode || {},
    uiState: context.uiState || {
      disclosures: new Map(),
      oneOfBranches: new Map(),
      oneOfSelection: new Map(),
    },
    config: {
      ...config,
      layout: {
        groups: {},
        ...(config.layout || {}),
      },
      visibility: {
        hiddenPaths: [],
        customVisibility: () => true,
        ...(config.visibility || {}),
      },
      sorting: {
        defaultRenderLast: [],
        defaultPriority: [],
        perObjectPriority: {},
        ...(config.sorting || {}),
      },
    },
  };
}

const forms = (window as any).VanillaSchemaForms || VanillaSchemaForms;
const renderPropertiesCompat = typeof (forms as any).renderProperties === "function"
  ? (forms as any).renderProperties.bind(forms)
  : renderProperties;

Object.assign(forms.rendererConfig.classes, {
  buttonPrimary: "wa-native-button wa-native-button--brand",
  buttonSecondary: "wa-native-button wa-native-button--neutral",
  buttonDanger: "wa-native-button wa-native-button--danger",
  checkboxRow: "mqb-checkbox-row",
});

forms.setI18n({
  keys: {
    Map_of_Route: "Routes",
  },
});

forms.setConfig({
  visibility: {
    hiddenPaths: ["publishers", "consumers", "routes", "presets", "history"],
    customVisibility: (node: SchemaNode, path: string) => {
      const description = String(node.description || "");
      const lowerPath = path.toLowerCase();
      const formMode = String((window as any)._mqb_form_mode || "");
      const fieldName = lowerPath.split(".").pop() || "";

      if (formMode === "publisher" && ["id", "presets", "url", "method", "queue", "topic", "database", "path", "collection", "payload", "headers", "sort_order"].includes(fieldName)) {
        return false;
      }

      // These top-level consumer fields are handled by dedicated tabs or UI elements, so hide them from the main definition form.
      if (formMode === "consumer" && ["id", "presets", "output",  "response", "message_capture"].includes(fieldName)) {
        return false;
      }

      const isPubCtx = lowerPath.includes("publishers") || lowerPath.includes(".output") || formMode === "publisher";
      const isConsCtx = lowerPath.includes("consumers") || lowerPath.includes(".input") || formMode === "consumer";

      if (isPubCtx && /\(Consumer only\)/i.test(description)) {
        return false;
      }
      if (isConsCtx && /\(Publisher only\)/i.test(description)) {
        return false;
      }

      return true;
    },
  },
  sorting: {
    defaultRenderLast: ["middlewares"],
    defaultPriority: [
      "input",
      "output",
      "name",
      "title",
      "type",
      "enabled",
      "active",
      "url",
      "brokers",
      "username",
      "password",
      "topic",
      "group",
      "path",
      "key",
      "value",
      "required",
      "routes",
      "endpoint",
    ],
  },
});

function createWrappedContainer(content: Node, className = "") {
  const wrapper = document.createElement("div");
  wrapper.className = className;
  wrapper.appendChild(content);
  return wrapper;
}

function asNode(value: Node | null): Node {
  return value || document.createDocumentFragment();
}

function addClassTokens(element: Element | null, ...classNames: Array<string | undefined>) {
  if (!element) return;
  const tokens = classNames
    .flatMap((value) => String(value || "").split(/\s+/))
    .map((value) => value.trim())
    .filter(Boolean);

  if (tokens.length > 0) {
    element.classList.add(...tokens);
  }
}

function stripClassTokens(element: Element | null, ...classNames: Array<string | undefined>) {
  if (!element) return;
  const tokens = classNames
    .flatMap((value) => String(value || "").split(/\s+/))
    .map((value) => value.trim())
    .filter(Boolean);

  if (tokens.length > 0) {
    element.classList.remove(...tokens);
  }
}

function sanitizeCheckboxControl(input: HTMLInputElement, inputElement: HTMLElement) {
  [input, inputElement, input.parentElement, input.closest(".form-check")].forEach((element) => {
    stripClassTokens(
      element,
      "form-check",
      "form-check-input",
      "form-switch",
      "mb-3",
      "vsf-wa-field",
      rendererConfig.classes.compactRow,
      rendererConfig.classes.compactContent,
      rendererConfig.classes.compactLabel,
    );
  });
}

function formatLabel(node: SchemaNode, elementId: string) {
  const segments = String(elementId || "").split(".");
  const fieldKey = segments[segments.length - 1] || "";
  const parentKey = segments[segments.length - 2] || "";
  const indexedFieldLabels: Record<string, string[]> = {
    basic_auth: ["Username", "Password"],
  };

  if (/^\d+$/.test(fieldKey)) {
    const explicitLabel = indexedFieldLabels[parentKey]?.[Number(fieldKey)];
    if (explicitLabel) return explicitLabel;
  }

  return formatWebAwesomeLabel(node, elementId);
}

function formatDescription(node: SchemaNode, _elementId: string) {
  const description = String(node.description || "");
  // Suppress technical internal descriptions that sometimes leak into the UI via shared schema refs
  if (description.toLowerCase().includes("routeoptions")) {
    return "";
  }
  return description;
}

// Temporarily swap in the UI-formatted description while the underlying renderer runs,
// then restore the original so the schema node is left untouched.
function withFormattedDescription<T>(node: SchemaNode, elementId: string, render: () => T): T {
  const originalDescription = node.description;
  node.description = formatDescription(node, elementId);
  try {
    return render();
  } finally {
    node.description = originalDescription;
  }
}

function parseElementPath(pathLike: string): Array<string | number> {
  const path = String(pathLike || "");
  const parts: Array<string | number> = [];
  const pattern = /([^[.\]]+)|\[(\d+|[^[\]]+)\]/g;

  for (const match of path.matchAll(pattern)) {
    const segment = match[1] ?? match[2];
    if (!segment) continue;
    parts.push(/^\d+$/.test(segment) ? Number(segment) : segment);
  }

  return parts;
}

const baseRenderBoolean = typeof domRenderer.renderBoolean === "function"
  ? domRenderer.renderBoolean.bind(domRenderer)
  : null;

if (baseRenderBoolean) {
  domRenderer.renderBoolean = (
    node: SchemaNode,
    elementId: string,
    inputName: string,
    ...rest: unknown[]
  ) => {
    const rendered = baseRenderBoolean(node, elementId, inputName, ...rest) as HTMLElement;
    const input = rendered?.querySelector?.('input[type="checkbox"]') as HTMLInputElement | null;

    if (!input) {
      return rendered;
    }

    sanitizeCheckboxControl(input, rendered);
    input.classList.add("wa-checkbox");
    input.addEventListener("change", () => {
      const activeStore = (forms as any).__activeStore;
      if (activeStore?.setPath) {
        activeStore.setPath(parseElementPath(input.name || elementId), input.checked);
      }
    });

    return renderSvelteNode(CheckboxField, {
      label: formatLabel(node, elementId),
      description: formatDescription(node, elementId),
      labelFor: input.id || elementId,
      control: input,
    });
  };
}

function addDesktopFilePicker(
  renderedField: HTMLElement,
  input: HTMLInputElement,
  formMode: unknown,
) {
  const parent = input.parentNode;
  if (!parent) return;

  const control = document.createElement("div");
  control.className = "mqb-file-path-control";
  parent.insertBefore(control, input);
  control.appendChild(input);

  const button = document.createElement("button");
  button.type = "button";
  button.className = "wa-native-button wa-native-button--neutral mqb-file-path-browse";
  button.textContent = "Browse…";
  button.title = formMode === "consumer" ? "Select an input file" : "Select an output file";
  button.setAttribute("aria-label", button.title);
  button.addEventListener("click", async () => {
    button.disabled = true;
    try {
      const selected = await pickFilePath(formMode === "consumer" ? "open" : "save", input.value);
      if (selected === null) return;

      input.value = selected;
      input.dispatchEvent(new Event("input", { bubbles: true }));
      input.focus();
    } finally {
      button.disabled = false;
    }
  });
  control.appendChild(button);

  renderedField.classList.add("mqb-file-path-field");
}

domRenderer.renderFieldWrapper = (
  node: SchemaNode,
  elementId: string,
  inputElement: HTMLElement,
  wrapperClass?: string,
) => {
  const input = (inputElement.querySelector?.("input, select, textarea") as HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement | null)
    || inputElement;
  const isCheckbox = input instanceof HTMLInputElement && input.type === "checkbox";
  const isPassword = input instanceof HTMLInputElement && node.format === "password";

  if (isPassword) {
    input.type = "password";
    input.autocomplete = input.autocomplete || "current-password";
  }

  if (input?.classList) {
    if (isCheckbox) {
      sanitizeCheckboxControl(input as HTMLInputElement, inputElement);
      input.classList.add("wa-checkbox");
    } else {
      input.classList.add("field-input");
    }

    const isTechnical = ["url", "brokers", "topic", "group"].some((token) =>
      elementId.toLowerCase().includes(token),
    );
    if (!isCheckbox && isTechnical) {
      input.style.fontFamily = "var(--font)";
    }
  }

  if (node["wa-no-label"] === true) {
    inputElement.setAttribute("wa-no-label", "true");
  }

  if (node.type === "object" || node.type === "array" || node.oneOf) {
    return createWrappedContainer(inputElement, wrapperClass || "");
  }

  if (isCheckbox) {
    return renderSvelteNode(CheckboxField, {
      label: formatLabel(node, elementId),
      description: formatDescription(node, elementId),
      labelFor: input instanceof HTMLElement ? input.id : undefined,
      control: input,
    });
  }

  if (isPassword) {
    return renderSvelteNode(PasswordField, {
      label: formatLabel(node, elementId),
      description: formatDescription(node, elementId),
      labelFor: input instanceof HTMLElement ? input.id : undefined,
      control: inputElement,
      fieldName: elementId,
      wrapperClass: wrapperClass || "",
      required: Boolean(node.required),
      maskedLabel: "Show",
      visibleLabel: "Hide",
    });
  }

  const renderedField = renderSvelteNode(FormField, {
    label: formatLabel(node, elementId),
    description: formatDescription(node, elementId),
    labelFor: input instanceof HTMLElement ? input.id : undefined,
    control: inputElement,
    wrapperClass: wrapperClass || "",
    required: Boolean(node.required),
  });

  if (
    appShell.isDesktop()
    && input instanceof HTMLInputElement
    && node["mqb-file-path"] === true
  ) {
    addDesktopFilePicker(renderedField, input, (window as any)._mqb_form_mode);
  }

  return renderedField;
};

if (forms.domRenderer && forms.domRenderer !== domRenderer) {
  Object.assign(forms.domRenderer, domRenderer);
}

const tlsBaseRenderer = {
  render: (node: SchemaNode, _path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext) => {
    const renderContext = withRendererDefaults(context);
    const requiredNode = node.properties?.required;
    if (!requiredNode) {
      return renderObject(renderContext, node, elementId, false, dataPath);
    }

    const valuePath = [...dataPath, "required"];
    const checkboxId = `${elementId}.required`;
    const currentValue = Boolean(context.store.getPath(valuePath) ?? requiredNode.defaultValue);

    const restProperties = { ...(node.properties || {}) };
    delete restProperties.required;

    return renderSvelteNode(OptionalSectionField, {
      label: String(node.title || "Options"),
      description: formatDescription(requiredNode, elementId) || formatDescription(node, elementId),
      checked: currentValue,
      inputId: checkboxId,
      onToggle: (nextChecked: boolean) => {
        context.store.setPath(valuePath, nextChecked);
      },
      content: createWrappedContainer(
        renderPropertiesCompat(renderContext, restProperties, elementId, dataPath),
        "mqb-optional-section-fields",
      ),
    });
  },
};

const getArrayItemNameField = (container: ParentNode | null) =>
  container?.querySelector?.(
    'input[id$=".name"], input[name$="[name]"], input[name="name"], textarea[id$=".name"], textarea[name$="[name]"], textarea[name="name"]',
  ) || null;

const getArrayItemLegend = (arrayItem: ParentNode | null) => {
  if (!(arrayItem instanceof Element)) return null;
  const directContent = arrayItem.querySelector(`:scope > .${rendererConfig.triggers.arrayItemContent}`);
  return directContent?.querySelector?.(":scope > fieldset > legend") || arrayItem.querySelector("legend");
};

const updateArrayItemLabel = (arrayItem: ParentNode | null) => {
  const legend = getArrayItemLegend(arrayItem);
  if (!(arrayItem instanceof Element) || !legend) return;

  if (arrayItem.getAttribute("wa-no-label") === "true") {
    legend.textContent = "";
    (legend as HTMLElement).style.display = "none";
    return;
  }

  const nameField = getArrayItemNameField(arrayItem);
  if (!nameField) return;

  const currentLabel = String(legend.textContent || "").trim();
  const fallbackLabel = arrayItem.getAttribute("data-default-label") || currentLabel || "Item";

  if (!arrayItem.getAttribute("data-default-label") && /^Item\s+\d+$/i.test(currentLabel)) {
    arrayItem.setAttribute("data-default-label", currentLabel);
  }

  legend.textContent = String((nameField as HTMLInputElement).value || "").trim() || fallbackLabel;
};

const syncNamedArrayItemLabels = (root: ParentNode = document) => {
  root.querySelectorAll?.(`.${rendererConfig.triggers.arrayItemRow}`).forEach((row) => {
    updateArrayItemLabel(row);
  });
};

domRenderer.renderArrayItem = (content: Node, options?: { isRemovable?: boolean }) => {
  const element = renderSvelteNode(ArrayItemRow, {
    content,
    removable: options?.isRemovable !== false,
    removeClassName: `${rendererConfig.classes.buttonDanger} ${rendererConfig.triggers.removeArrayItem} mqb-array-item-delete`,
  });

  addClassTokens(
    element,
    rendererConfig.classes.arrayItemRow,
    rendererConfig.triggers.arrayItemRow,
    "mqb-array-item-row",
  );

  const contentHost = element.querySelector(".mqb-array-item-content");
  addClassTokens(
    contentHost,
    rendererConfig.classes.arrayItemContent,
    rendererConfig.triggers.arrayItemContent,
  );

  const legend = element.querySelector("legend");
  if (legend?.textContent?.trim()) {
    element.setAttribute("data-default-label", legend.textContent.trim());
  }

  if (content instanceof HTMLElement && (content.hasAttribute("wa-no-label") || content.querySelector('[wa-no-label="true"]'))) {
    element.setAttribute("wa-no-label", "true");
  }

  window.setTimeout(() => updateArrayItemLabel(element), 0);
  window.setTimeout(() => updateArrayItemLabel(element), 50);
  return element;
};

if (!(window as any)._mqb_array_item_name_sync_installed) {
  (window as any)._mqb_array_item_name_sync_installed = true;

  document.addEventListener("input", (event) => {
    const input = (event.target as Element | null)?.closest?.(
      'input[id$=".name"], input[name$="[name]"], input[name="name"]',
    );
    if (!input) return;
    updateArrayItemLabel(input.closest(`.${rendererConfig.triggers.arrayItemRow}`));
  });

  const target = document.body || document.documentElement;
  if (target instanceof Node) {
    const observer = new MutationObserver((mutations) => {
      for (const mutation of mutations) {
        mutation.addedNodes.forEach((node) => {
          if (!(node instanceof Element)) return;
          if (node.matches(`.${rendererConfig.triggers.arrayItemRow}`)) {
            updateArrayItemLabel(node);
            window.setTimeout(() => updateArrayItemLabel(node), 50);
            return;
          }
          syncNamedArrayItemLabels(node);
          window.setTimeout(() => syncNamedArrayItemLabels(node), 50);
        });
      }
    });

    observer.observe(target, { childList: true, subtree: true });
  }
}

function fixNullBooleans(node: SchemaNode, dataPath: Array<string | number>, context: RendererContext) {
  if (!node.properties) return;
  const store = context.store;
  const data = store.getPath(dataPath);

  if (!data || typeof data !== "object") return;

  const hasNullBooleans = Object.entries(node.properties as Record<string, SchemaNode>).some(([key, prop]) => prop?.type === "boolean" && data[key] === null);
  if (!hasNullBooleans) return;

  window.setTimeout(() => {
    const currentData = { ...(store.getPath(dataPath) || {}) };
    let changed = false;

    for (const [key, prop] of Object.entries(node.properties as Record<string, SchemaNode>)) {
      if (prop?.type === "boolean" && currentData[key] === null) {
        currentData[key] = false;
        changed = true;
      }
    }

    if (changed) {
      store.setPath(dataPath, currentData);
    }
  }, 0);
}

const tlsRenderer = {
  render: (node: SchemaNode, path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext) => {
    fixNullBooleans(node, dataPath, context);
    const element = tlsBaseRenderer.render(node, path, elementId, dataPath, context) as HTMLElement;
    element?.classList?.add("ui_tls");
    return element;
  },
};

const basicAuthRenderer = {
  render: (_node: SchemaNode, _path: string, _elementId: string, dataPath: Array<string | number>, context: RendererContext) => {
    const store = context.store;
    const storePath = toStorePath(dataPath);
    const currentValue = store.getPath(storePath);
    const tuple = Array.isArray(currentValue) ? currentValue : ["", ""];

    return renderSvelteNode(BasicAuthEditor, {
      title: "Basic Auth",
      description: "",
      username: String(tuple[0] || ""),
      password: String(tuple[1] || ""),
      onChange: ({ username, password }: { username: string; password: string }) => {
        const next = [username, password];
        const hasAnyValue = next.some((item) => item.trim().length > 0);
        store.setPath(storePath, hasAnyValue ? next : null);
      },
    });
  },
};

// Both custom headers and env vars are a sorted key/value map edited through HeadersEditor;
// they differ only in labels, so share one renderer factory.
const createKeyValueRenderer = (editorProps: (node: SchemaNode) => Record<string, unknown>) => ({
  render: (node: SchemaNode, _path: string, _elementId: string, dataPath: Array<string | number>, context: RendererContext) => {
    const store = context.store;
    const storePath = toStorePath(dataPath);
    const currentValue = store.getPath(storePath) ?? node.defaultValue ?? {};
    const rows = Object.entries(currentValue)
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([key, value]) => ({ key: String(key), value: String(value ?? "") }));

    return renderSvelteNode(HeadersEditor, {
      description: formatDescription(node, _elementId),
      rows,
      ...editorProps(node),
      onChange: (nextRows: Array<{ key: string; value: string }>) => {
        const nextValue = Object.fromEntries(
          nextRows
            .filter((row) => row.key.trim().length > 0)
            .map((row) => [row.key.trim(), row.value]),
        );
        store.setPath(storePath, nextValue);
      },
    });
  },
});

const customHeadersRenderer = createKeyValueRenderer((node) => ({
  title: String(node.title || "Custom Headers"),
}));

const envVarsRenderer = createKeyValueRenderer(() => ({
  title: "Environment Variables",
  sectionLabel: "",
  keyPlaceholder: "Variable name",
  valuePlaceholder: "Value",
  addLabel: "Add Variable",
  emptyLabel: "No environment variables defined.",
  deleteLabel: "Delete",
}));

// Tuning knobs shared by consumers and routes. Only `batch_size` is worth surfacing
// by default; the rest are rarely touched and otherwise crowd out the endpoint itself.
const ADVANCED_ROUTE_OPTION_FIELDS = [
  "concurrency",
  "commit_concurrency_limit",
  "startup_timeout_ms",
  "reconnect_interval_ms",
  "empty_batch_delay_ms",
  "allow_fault_injection",
  "exit_on_empty",
];

function renderObjectWithAdvancedFields(
  context: RendererContext,
  node: SchemaNode,
  elementId: string,
  dataPath: Array<string | number>,
  advancedFields: readonly string[],
): Node {
  const properties = node.properties || {};
  const advancedKeys = advancedFields.filter((key) => key in properties);
  if (advancedKeys.length === 0) {
    return asNode(renderObject(context, node, elementId, false, dataPath));
  }

  const visibleProperties = { ...properties };
  const advancedProperties: Record<string, SchemaNode> = {};
  for (const key of advancedKeys) {
    advancedProperties[key] = properties[key];
    delete visibleProperties[key];
  }

  return renderSvelteNode(CollapsibleFields, {
    visibleContent: asNode(
      renderObject(context, { ...node, properties: visibleProperties }, elementId, false, dataPath),
    ),
    hiddenContent: createWrappedContainer(
      renderPropertiesCompat(context, advancedProperties, elementId, dataPath),
      "mqb-form-block",
    ),
    toggleLabel: "Show advanced options",
  });
}

const routeObjectRenderer = {
  render: (node: SchemaNode, _path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext) =>
    withFormattedDescription(node, elementId, () =>
      renderObjectWithAdvancedFields(context, node, elementId, dataPath, ADVANCED_ROUTE_OPTION_FIELDS),
    ),
};

const routesRenderer = {
  render: (node: SchemaNode, _path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext) => {
    const wrapper = domRenderer.renderAdditionalProperties(
      { ...node, defaultValue: {} },
      elementId,
      { title: null },
    ) as HTMLElement;

    const itemsContainer = wrapper.querySelector(`.${rendererConfig.triggers.additionalPropertyItems}`);
    const currentRoutes = context.store.getPath(dataPath) ?? node.defaultValue;

    if (itemsContainer && node.additionalProperties && currentRoutes && typeof currentRoutes === "object") {
      const definedProps = new Set(node.properties ? Object.keys(node.properties) : []);
      let apIndex = 0;

      Object.keys(currentRoutes).forEach((key) => {
        if (definedProps.has(key)) return;

        const valueNode = hydrateNodeWithData(node.additionalProperties, currentRoutes[key]);
        const routePath = `${elementId}.__ap_${apIndex}`;
        const routeDataPath = [...dataPath, key];
        const valueHtml = renderNode(context, valueNode, routePath, true, routeDataPath);
        const rowNode = routesRenderer.renderAdditionalPropertyRow(
          asNode(valueHtml),
          key,
          `${routePath}_key`,
        );

        itemsContainer.appendChild(rowNode);
        apIndex += 1;
      });
    }

    return domRenderer.renderObject(node, elementId, wrapper);
  },
  getDefaultKey: (index: number) => `Route ${index + 1}`,
  renderAdditionalPropertyRow: (valueHtml: Node, defaultKey: string, uniqueId: string) =>
    renderSvelteNode(AdditionalPropertyRow, {
      inputId: uniqueId,
      value: defaultKey,
      placeholder: "Route name",
      valueContent: valueHtml,
      removeClassName: `btn-remove-ap ${rendererConfig.classes.buttonDanger} ${rendererConfig.triggers.removeAdditionalProperty}`,
    }),
};

domRenderer.renderAdditionalPropertyRow = (
  valueHtml: Node,
  defaultKey = "",
  uniqueId = "",
) => renderSvelteNode(GenericAdditionalPropertyRow, {
  inputId: uniqueId || undefined,
  value: defaultKey,
  placeholder: "Key",
  valueContent: valueHtml,
  removeClassName: `${rendererConfig.classes.buttonDanger} ${rendererConfig.triggers.removeAdditionalProperty} mqb-ap-delete`,
});

const createEndpointRenderer = (type: string) => ({
  render: (node: SchemaNode, _path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext) => {
    fixNullBooleans(node, dataPath, context);

    const basicFields = BASIC_ENDPOINT_FIELDS[type] || [];
    const allProperties = node.properties || {};
    if (type === "file" && allProperties.path) {
      allProperties.path["mqb-file-path"] = true;
    }
    const visibleProperties: Record<string, SchemaNode> = {};
    const hiddenProperties: Record<string, SchemaNode> = {};

    Object.keys(allProperties).forEach((key) => {
      if (basicFields.includes(key)) visibleProperties[key] = allProperties[key];
      else hiddenProperties[key] = allProperties[key];
    });

    const hiddenKeys = Object.keys(hiddenProperties);
    const hasOneOf = Array.isArray(node.oneOf) && node.oneOf.length > 0;

    if (hiddenKeys.length === 0 && !hasOneOf) {
      return renderObject(context, node, elementId, false, dataPath);
    }

    const visibleFragment = renderPropertiesCompat(context, visibleProperties, elementId, dataPath);
    if (hasOneOf) {
      const oneOfFragment = domRenderer.renderOneOf(node, elementId);
      if (oneOfFragment) visibleFragment.prepend(oneOfFragment);
    }

    const visibleWrapper = createWrappedContainer(visibleFragment, "mqb-form-block");

    const hiddenFragment = document.createDocumentFragment();
    for (const key of hiddenKeys) {
      const specialRenderer = (CUSTOM_RENDERERS as any)[key];
      if (specialRenderer && typeof specialRenderer.render === "function") {
        hiddenFragment.appendChild(specialRenderer.render(hiddenProperties[key], `${_path}.${key}`, `${elementId}.${key}`, [...dataPath, key], context));
      } else {
        hiddenFragment.appendChild(asNode(renderNode(context, hiddenProperties[key], `${elementId}.${key}`, false, [...dataPath, key])));
      }
    }
    const hiddenWrapper = createWrappedContainer(hiddenFragment, "mqb-form-block");

    return renderSvelteNode(CollapsibleFields, {
      description: formatDescription(node, elementId),
      visibleContent: visibleWrapper,
      hiddenContent: hiddenWrapper,
      toggleLabel: "Show advanced options",
    });
  },
});

const createScalarEndpointRenderer = (
  type: "static" | "ref",
  options: { title: string; placeholder: string; suggestions?: () => string[] },
) => ({
  render: (node: SchemaNode, _path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext) => {
    const currentValue = getPathValue(context.data, dataPath);
    const suggestions = options.suggestions ? options.suggestions() : [];

    return renderSvelteNode(ScalarEndpointInput, {
      title: node.title === "" ? "" : String(node.title || options.title),
      description: String(node.description || ""),
      value: deriveScalarEndpointValue(currentValue, type, node),
      placeholder: options.placeholder,
      suggestions,
      name: elementId,
      onChange: (next: string) => commitScalarEndpointValue(context, dataPath, node, type, next),
    });
  },
});

const rootRenderer = {
  render: (node: SchemaNode, _path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext) =>
    withFormattedDescription(node, elementId, () =>
      createWrappedContainer(
        renderObjectWithAdvancedFields(context, node, elementId, dataPath, ADVANCED_ROUTE_OPTION_FIELDS),
        "mqb-form-block",
      ),
    ),
};


const baseMiddlewaresRenderer = createTypeSelectArrayRenderer({
  buttonLabel: "Add Middleware",
  itemLabel: "Middleware",
});

const middlewaresRenderer = {
  render: (node: SchemaNode, path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext, isHeadless = false) => {
    const element = (baseMiddlewaresRenderer as unknown as CustomRenderer).render(node, path, elementId, dataPath, context, isHeadless) as HTMLElement;
    const toggleButton = element.querySelector(`.${rendererConfig.triggers.arrayTypeToggle}`) as HTMLElement | null;
    const typeSelect = element.querySelector(`.${rendererConfig.triggers.arrayTypeSelect}`) as HTMLSelectElement | null;

    // The library only makes the picker its own trigger where select.showPicker() is missing
    // (WebKit, so Safari and Tauri); elsewhere it keeps an "Add Middleware" button that reveals
    // the select. Apply the same shape everywhere so desktop and browser look alike. Removing
    // the button is what keeps the picker visible: the library hides it again only when there
    // is a button to swap back to.
    if (toggleButton && typeSelect) {
      const placeholder = typeSelect.querySelector("option[value='']");
      if (placeholder) placeholder.textContent = toggleButton.textContent || "Add Middleware";

      toggleButton.remove();
      addClassTokens(typeSelect, rendererConfig.classes.buttonPrimary);
      typeSelect.style.display = "inline-block";
      // Size to the label, not to the longest middleware name a plugin adds to the list.
      typeSelect.style.setProperty("field-sizing", "content");
    }

    return element;
  },
};

const getPathValue = (root: any, path: Array<string | number>) => path.reduce((current, segment) => current?.[segment], root);

const setPathValue = (root: any, path: Array<string | number>, value: unknown) => {
  if (path.length === 0) {
    if (Array.isArray(root) && Array.isArray(value)) {
      root.splice(0, root.length, ...value);
      return;
    }

    if (root && typeof root === "object" && value && typeof value === "object" && !Array.isArray(value)) {
      Object.keys(root).forEach((key) => {
        delete root[key];
      });
      Object.assign(root, value);
      return;
    }

    throw new Error("Cannot replace root form value with a non-object payload");
  }

  let cursor = root;
  path.slice(0, -1).forEach((segment, index) => {
    const nextSegment = path[index + 1];
    if (cursor[segment] === undefined || cursor[segment] === null) {
      cursor[segment] = typeof nextSegment === "number" ? [] : {};
    }
    cursor = cursor[segment];
  });
  cursor[path[path.length - 1]] = value;
};

const getName = (dataPath: Array<string | number>) => dataPath.map(String).join('.');

const commitPathValue = (
  context: RendererContext,
  dataPath: Array<string | number>,
  value: unknown,
) => {
  if (context.store && typeof context.store.setPath === "function") {
    context.store.setPath(dataPath, value);
    return;
  }

  const newData = structuredClone(context.data);
  setPathValue(newData, dataPath, value);
  context.onChange(newData);
};

// A scalar endpoint (static/ref) may be stored either as a bare string or as an
// object keyed by `key` (e.g. { ref: "..." }); derive the editable string from either shape.
const deriveScalarEndpointValue = (currentValue: any, key: string, node: SchemaNode): string => {
  if (typeof currentValue === "string") return currentValue;
  if (currentValue && typeof currentValue === "object" && !Array.isArray(currentValue) && typeof currentValue[key] === "string") {
    return currentValue[key];
  }
  return typeof node.defaultValue === "string" ? node.defaultValue : "";
};

// Write a scalar endpoint back, preserving the object shape when the existing value
// (or schema) is an object and writing a bare string otherwise.
const commitScalarEndpointValue = (
  context: RendererContext,
  dataPath: Array<string | number>,
  node: SchemaNode,
  key: string,
  next: string,
) => {
  const existing = getPathValue(context.data, dataPath);
  const keepObjectShape =
    node.type === "object" || (existing && typeof existing === "object" && !Array.isArray(existing) && key in existing);
  const updated = keepObjectShape
    ? { ...(existing && typeof existing === "object" ? existing : {}), [key]: next }
    : next;
  commitPathValue(context, dataPath, updated);
};

const descriptionRenderer = {
  render: (node: SchemaNode, _path: string, elementId: string, dataPath: Array<string | number>) => {
    const textarea = document.createElement("textarea");
    textarea.className = rendererConfig.classes.input;
    textarea.id = elementId;
    textarea.name = getName(dataPath);
    textarea.rows = 3;
    textarea.value = node.defaultValue !== undefined ? String(node.defaultValue) : "";
    if (node.readOnly) textarea.disabled = true;
    if (node.required) textarea.required = true;
    return domRenderer.renderFieldWrapper(node, elementId, textarea);
  },
};

const endpointRenderer = {
  render: (node: SchemaNode, _path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext) => {
    // Primary publisher and consumer endpoint definitions should use the full object editor
    // instead of being rendered as a reference to another publisher.
    // Use _path for more robust matching of nested elements.
    const isPrimaryDefinition =
      elementId === "root.endpoint" ||
      elementId === "root.input" ||
      elementId === "root.output" ||
      elementId === "input" ||
      elementId === "output" ||
      /^publishers\.\d+\.endpoint$/i.test(_path) || /^consumers\.\d+\.(input|output|endpoint)$/i.test(_path);

    if (isPrimaryDefinition) {
      return renderObject(context, node, elementId, false, dataPath);
    }

    const currentValue = getPathValue(context.data, dataPath);

    return renderSvelteNode(Endpoint, {
      title: node.title === "" ? "" : String(node.title || "Ref Endpoint"),
      description: String(node.description || ""),
      value: deriveScalarEndpointValue(currentValue, "ref", node),
      name: elementId,
      onChange: (next: string) => commitScalarEndpointValue(context, dataPath, node, "ref", next),
    });
  },
};

// Transform's inline schema is a free-form JSON document, so it has no `type` in the config
// schema and the library would render a text input and store whatever was typed as a string -
// which the engine then refuses to start with ("schema must be a JSON object"). Edit it as JSON
// and only ever write a parsed object.
const transformSchemaRenderer = {
  render: (node: SchemaNode, _path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext) => {
    const store = context.store;
    const storePath = toStorePath(dataPath);
    const current = store.getPath(storePath);

    const textarea = document.createElement("textarea");
    textarea.className = rendererConfig.classes.input;
    textarea.id = elementId;
    textarea.rows = 10;
    textarea.spellcheck = false;
    textarea.style.fontFamily = "monospace";
    textarea.placeholder = '{ "type": "object", "properties": { … } }';
    textarea.value = current === undefined ? "" : JSON.stringify(current, null, 2);

    const message = document.createElement("div");
    message.className = "mqb-json-error";

    const commit = () => {
      const raw = textarea.value.trim();
      let error = "";

      if (raw === "") {
        store.removePath ? store.removePath(storePath) : store.setPath(storePath, undefined);
      } else {
        try {
          const parsed = JSON.parse(raw);
          if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
            error = "The schema must be a JSON object.";
          } else {
            store.setPath(storePath, parsed);
          }
        } catch (parseError) {
          error = `Invalid JSON: ${(parseError as Error).message}`;
        }
      }

      message.textContent = error;
      textarea.classList.toggle("mqb-json-invalid", Boolean(error));
    };

    // The library collects field values by element id through delegated listeners on the form
    // container; left to bubble, they would store the raw text alongside the parsed value.
    textarea.addEventListener("input", (event) => event.stopPropagation());
    textarea.addEventListener("change", (event) => {
      event.stopPropagation();
      commit();
    });

    const field = domRenderer.renderFieldWrapper(node, elementId, textarea) as HTMLElement;
    field.appendChild(message);
    return field;
  },
};

// `filter` is a bare expression or a map with `on_error`. The library shows neither value for
// that union, so edit it as two fields and keep the bare form unless `on_error` is needed.
const filterRenderer = {
  render: (node: SchemaNode, _path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext) => {
    const store = context.store;
    const storePath = toStorePath(dataPath);
    const current = store.getPath(storePath);
    const isMap = Boolean(current) && typeof current === "object" && !Array.isArray(current);
    const mapSchema = (node.oneOf || []).find((branch: SchemaNode) => branch.properties)?.properties || {};

    const expression = document.createElement("input");
    expression.type = "text";
    expression.className = rendererConfig.classes.input;
    expression.id = `${elementId}.expression`;
    expression.spellcheck = false;
    expression.placeholder = "amount > 100";
    expression.value = String((isMap ? current.expression : current) ?? "");

    const onError = document.createElement("select");
    onError.className = rendererConfig.classes.input;
    onError.id = `${elementId}.on_error`;
    for (const option of ["fail", "drop"]) {
      onError.appendChild(new Option(option, option));
    }
    onError.value = isMap && current.on_error === "drop" ? "drop" : "fail";

    const commit = () => {
      const next = isMap || onError.value !== "fail"
        ? { expression: expression.value, on_error: onError.value }
        : expression.value;
      store.setPath(storePath, next);
    };

    // Left to bubble, the library would also store each field under its own element id.
    for (const control of [expression, onError]) {
      control.addEventListener("input", (event) => event.stopPropagation());
      control.addEventListener("change", (event) => {
        event.stopPropagation();
        commit();
      });
    }

    const fields = document.createDocumentFragment();
    fields.append(
      domRenderer.renderFieldWrapper(
        { ...mapSchema.expression, type: "string", title: "Expression", required: true },
        expression.id,
        expression,
      ),
      domRenderer.renderFieldWrapper(
        { ...mapSchema.on_error, type: "string", title: "On Error" },
        onError.id,
        onError,
      ),
    );
    return domRenderer.renderObject(node, elementId, createWrappedContainer(fields, "mqb-form-block"));
  },
};

const transformMappingRenderer = {
  render: (node: SchemaNode, _path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext) => {
    const storePath = toStorePath(dataPath);
    const currentValue = storePath.length > 0 && context.store.getPath(storePath);
    const mapping = currentValue && typeof currentValue === "object" && !Array.isArray(currentValue)
      ? currentValue as Record<string, unknown>
      : {};
    const rows = Object.entries(mapping)
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([key, value]) => ({
        key,
        originalKey: key,
        value: typeof value === "string"
          ? value
          : String((value as Record<string, unknown> | null)?.path ?? ""),
      }));

    return renderSvelteNode(HeadersEditor, {
      title: "Mapping",
      sectionLabel: "",
      description: formatDescription(node, elementId),
      rows,
      keyPlaceholder: "Output field",
      valuePlaceholder: "Source path",
      addLabel: "Add Mapping",
      emptyLabel: "No mappings defined.",
      deleteLabel: "Delete",
      onChange: (nextRows: Array<{ key: string; originalKey?: string; value: string }>) => {
        const nextMapping = Object.fromEntries(
          nextRows
            .filter((row) => row.key.trim().length > 0)
            .map((row) => {
              const key = row.key.trim();
              // Renaming a row keeps its `default`/`required` siblings, which a
              // lookup by the new key would silently drop.
              const previous = mapping[row.originalKey ?? key];
              return [
                key,
                previous && typeof previous === "object" && !Array.isArray(previous)
                  ? { ...previous, path: row.value }
                  : row.value,
              ];
            }),
        );
        context.store.setPath(storePath, nextMapping);
      },
    });
  },
};

const CUSTOM_RENDERERS: Record<string, unknown> = {
  root: rootRenderer,
  AppConfig: rootRenderer,
  Route: routeObjectRenderer,
  route: routeObjectRenderer,
  Endpoint: endpointRenderer,
  EndpointConfig: endpointRenderer,
  tls: tlsRenderer,
  basic_auth: basicAuthRenderer,
  custom_headers: customHeadersRenderer,
  env_vars: envVarsRenderer,
  routes: routesRenderer,
  middlewares: middlewaresRenderer,
  description: descriptionRenderer,
  filter: filterRenderer,
  "transform.mapping": transformMappingRenderer,
  "transform.schema": transformSchemaRenderer,
  "root.endpoint": renderObject,
  "output.mode": { render: () => document.createDocumentFragment() },
  value: {
    render: (node: SchemaNode, _path: string, elementId: string, dataPath: Array<string | number>, context: RendererContext) => {
      if (elementId.startsWith("Routes.")) {
        const props = node.properties
          ? renderPropertiesCompat(context, node.properties, elementId, dataPath)
          : document.createDocumentFragment();
        const ap = domRenderer.renderAdditionalProperties(node, elementId);
        const oneOf = domRenderer.renderOneOf(node, elementId);
        const content = document.createDocumentFragment();
        content.append(props, asNode(ap), asNode(oneOf));
        return domRenderer.renderHeadlessObject(elementId, content);
      }
      return withFormattedDescription(node, elementId, () =>
        renderObject(context, node, elementId, false, dataPath),
      );
    },
  },
};

[
  "aws",
  "kafka",
  "nats",
  "file",
  "memory",
  "amqp",
  "mongodb",
  "mqtt",
  "http",
  "static",
  "sled",
  "websocket",
  "sqlx",
  "grpc",
  "ibmmq",
  "redis_streams",
  "zeromq",
  "switch",
  "response",
  "custom",
].forEach((type) => {
  CUSTOM_RENDERERS[type] = createEndpointRenderer(type);
});

CUSTOM_RENDERERS.static = createScalarEndpointRenderer("static", {
  title: "Static",
  placeholder: "Static value",
});

CUSTOM_RENDERERS.ref = createScalarEndpointRenderer("ref", {
  title: "Ref",
  placeholder: "publisher_id",
  suggestions: () => collectPublisherIdSuggestions(appShell.config<Record<string, any>>()?.publishers),
});

CUSTOM_RENDERERS.RefConfig = CUSTOM_RENDERERS.ref;
CUSTOM_RENDERERS.StaticConfig = CUSTOM_RENDERERS.static;

forms.setCustomRenderers(CUSTOM_RENDERERS);
