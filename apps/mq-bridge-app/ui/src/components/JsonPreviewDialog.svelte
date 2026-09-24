<script lang="ts">
  import "@awesome.me/webawesome/dist/components/button/button.js";
  import { tick, onDestroy } from "svelte";
  import { EditorView, basicSetup } from "codemirror";
  import { Compartment } from "@codemirror/state";
  import { json, jsonParseLinter } from "@codemirror/lang-json";
  import { yaml } from "@codemirror/lang-yaml";
  import { linter, lintGutter, type Diagnostic } from "@codemirror/lint";
  import { parseDocument } from "yaml";
  import type { ConfigJsonVariant } from "../lib/import-export";
  import { formatConfigText, parseConfigText, type ConfigTextFormat } from "../lib/config-text";

  let {
    open = false,
    title = "JSON",
    value = undefined,
    variants = [],
    onClose = () => {},
    onApply = undefined,
  }: {
    open?: boolean;
    title?: string;
    value?: unknown;
    variants?: ConfigJsonVariant[];
    onClose?: () => void;
    // Resolving to `false` keeps the dialog open (e.g. a declined confirmation).
    onApply?: (variantId: string, value: Record<string, unknown>) => Promise<boolean | void> | boolean | void;
  } = $props();

  let dialogEl = $state<HTMLDialogElement | null>(null);
  let editorContainer = $state<HTMLElement | null>(null);
  let copyLabel = $state("Copy");
  let selectedVariantId = $state<string | null>(null);
  let format = $state<ConfigTextFormat>("json");
  let applyError = $state("");
  let applying = $state(false);
  let syntaxError = $state("");
  // Unapplied edits carried across a JSON/YAML switch.
  let draftValue = $state<Record<string, unknown> | undefined>(undefined);
  let editorView: EditorView | null = null;
  let renderedKey = "";
  let copyTimer: ReturnType<typeof setTimeout> | null = null;
  const languageCompartment = new Compartment();
  const editableCompartment = new Compartment();
  const lintCompartment = new Compartment();
  const lintJson = jsonParseLinter();

  function lintYaml(view: EditorView): Diagnostic[] {
    const length = view.state.doc.length;
    return parseDocument(view.state.doc.toString()).errors.map((error) => ({
      from: Math.min(error.pos[0], length),
      to: Math.min(Math.max(error.pos[1], error.pos[0] + 1), length),
      severity: "error",
      message: error.message,
    }));
  }

  function lintExtension(docFormat: ConfigTextFormat, isEditable: boolean) {
    if (!isEditable) return [];
    return linter((view) => {
      const diagnostics = docFormat === "yaml" ? lintYaml(view) : lintJson(view);
      const first = diagnostics[0];
      syntaxError = first ? `Line ${view.state.doc.lineAt(first.from).number}: ${first.message}` : "";
      return diagnostics;
    }, { delay: 300 });
  }

  const activeVariant = $derived(
    variants.length > 0 ? (variants.find((variant) => variant.id === selectedVariantId) ?? variants[0]) : null,
  );
  const displayValue = $derived(activeVariant ? activeVariant.value : value);
  const editable = $derived(Boolean(activeVariant?.editable && onApply));

  // A native dialog, not wa-dialog: WebKit misreports the selection inside slotted
  // shadow-DOM content, which made CodeMirror draw its cursor at the line start.
  $effect(() => {
    if (!dialogEl) return;
    if (open && !dialogEl.open) {
      dialogEl.showModal();
      editorView?.focus();
    }
    else if (!open && dialogEl.open) dialogEl.close();
  });

  $effect(() => {
    if (open) {
      void renderText(formatConfigText($state.snapshot(draftValue ?? displayValue), format), format, editable);
    }
  });

  async function renderText(doc: string, docFormat: ConfigTextFormat, isEditable: boolean) {
    await tick();
    if (!editorContainer) return;
    // Panels re-derive their variants on unrelated updates; re-rendering then would reset the cursor.
    const key = `${docFormat}|${isEditable}|${doc}`;
    if (editorView && key === renderedKey) return;
    renderedKey = key;
    syntaxError = "";
    const language = docFormat === "yaml" ? yaml() : json();
    if (!editorView) {
      editorView = new EditorView({
        doc,
        extensions: [
          basicSetup,
          languageCompartment.of(language),
          editableCompartment.of(EditorView.editable.of(isEditable)),
          lintGutter(),
          lintCompartment.of(lintExtension(docFormat, isEditable)),
          EditorView.theme({
            "&": { fontSize: "13px" },
            ".cm-scroller": { overflow: "auto" },
          }),
        ],
        parent: editorContainer,
      });
      editorView.focus();
      return;
    }
    editorView.dispatch({
      changes: { from: 0, to: editorView.state.doc.length, insert: doc },
      effects: [
        languageCompartment.reconfigure(language),
        editableCompartment.reconfigure(EditorView.editable.of(isEditable)),
        lintCompartment.reconfigure(lintExtension(docFormat, isEditable)),
      ],
    });
  }

  function currentText() {
    return editorView ? editorView.state.doc.toString() : formatConfigText(displayValue, format);
  }

  async function switchFormat(next: ConfigTextFormat) {
    if (next === format) return;
    if (!editable) {
      format = next;
      return;
    }
    try {
      draftValue = parseConfigText(currentText(), format);
      applyError = "";
      format = next;
    } catch (error) {
      applyError = `Cannot switch format: ${(error as Error).message}`;
    }
  }

  async function applyEdits() {
    if (applying || !onApply || !activeVariant) return;
    applying = true;
    applyError = "";
    try {
      if ((await onApply(activeVariant.id, parseConfigText(currentText(), format))) !== false) close();
    } catch (error) {
      applyError = (error as Error).message;
    } finally {
      applying = false;
    }
  }

  function selectVariant(id: string) {
    selectedVariantId = id;
    draftValue = undefined;
    applyError = "";
  }

  function close() {
    renderedKey = "";
    draftValue = undefined;
    applyError = "";
    onClose();
  }

  async function copyJson() {
    try {
      await navigator.clipboard.writeText(currentText());
      copyLabel = "Copied";
    } catch (error) {
      console.error("Failed to copy config to clipboard:", error);
      copyLabel = "Copy failed";
    }
    if (copyTimer) clearTimeout(copyTimer);
    copyTimer = setTimeout(() => {
      copyLabel = "Copy";
      copyTimer = null;
    }, 1200);
  }

  onDestroy(() => {
    if (copyTimer) {
      clearTimeout(copyTimer);
      copyTimer = null;
    }
    editorView?.destroy();
    editorView = null;
  });
</script>

<dialog bind:this={dialogEl} class="json-preview-dialog" aria-label={title} onclose={() => open && close()}>
  <header class="json-preview-header">
    <h2>{title}</h2>
    <button type="button" class="json-preview-close" aria-label="Close" onclick={close}>&times;</button>
  </header>
  <div bind:this={editorContainer} class="json-preview-container"></div>
  {#if applyError || syntaxError}
    <div class="json-preview-error" role="alert">{applyError || syntaxError}</div>
  {/if}
  <div class="json-preview-actions">
    <div class="json-preview-variants" role="group" aria-label="Syntax">
      {#each ["json", "yaml"] as const as option (option)}
        <wa-button
          size="small"
          variant={format === option ? "brand" : "neutral"}
          appearance={format === option ? "filled" : "outlined"}
          role="button"
          tabindex="0"
          onclick={() => void switchFormat(option)}
          onkeydown={(event: KeyboardEvent) => event.key === "Enter" && void switchFormat(option)}
          >{option.toUpperCase()}</wa-button
        >
      {/each}
    </div>
    {#if variants.length > 1}
      <div class="json-preview-variants" role="group" aria-label="Format">
        {#each variants as variant (variant.id)}
          <wa-button
            size="small"
            variant={activeVariant?.id === variant.id ? "brand" : "neutral"}
            appearance={activeVariant?.id === variant.id ? "filled" : "outlined"}
            role="button"
            tabindex="0"
            onclick={() => selectVariant(variant.id)}
            onkeydown={(event: KeyboardEvent) => event.key === "Enter" && selectVariant(variant.id)}
            >{variant.label}</wa-button
          >
        {/each}
      </div>
    {/if}
    <div class="json-preview-actions-right">
      <wa-button
        variant="neutral"
        appearance="outlined"
        role="button"
        tabindex="0"
        onclick={() => void copyJson()}
        onkeydown={(event: KeyboardEvent) => event.key === "Enter" && void copyJson()}>{copyLabel}</wa-button
      >
      {#if editable}
        <wa-button
          variant="success"
          role="button"
          tabindex="0"
          loading={applying}
          onclick={() => void applyEdits()}
          onkeydown={(event: KeyboardEvent) => event.key === "Enter" && void applyEdits()}>Apply</wa-button
        >
      {/if}
      <wa-button
        variant="brand"
        role="button"
        tabindex="0"
        onclick={close}
        onkeydown={(event: KeyboardEvent) => event.key === "Enter" && close()}>Close</wa-button
      >
    </div>
  </div>
</dialog>

<style>
  .json-preview-dialog {
    margin: auto;
    width: min(920px, calc(100vw - 32px));
    max-height: calc(100vh - 32px);
    padding: 20px 24px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--bg-panel);
    color: var(--text-primary);
    box-shadow: 0 12px 40px var(--shadow-color);
  }

  .json-preview-dialog[open] {
    display: flex;
    flex-direction: column;
    align-items: stretch;
  }

  .json-preview-dialog::backdrop {
    background: rgb(0 0 0 / 45%);
  }

  .json-preview-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 16px;
  }

  .json-preview-header h2 {
    margin: 0;
    font: 600 18px var(--font-ui);
  }

  .json-preview-close {
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 22px;
    line-height: 1;
    cursor: pointer;
  }

  .json-preview-close:focus:not(:focus-visible) {
    outline: none;
  }

  .json-preview-container {
    height: min(60vh, 640px);
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg-editor);
    overflow: hidden;
  }

  .json-preview-error {
    margin-top: 8px;
    color: var(--wa-color-danger-on-quiet, #c62828);
    font-size: 12px;
    white-space: pre-wrap;
  }

  .json-preview-actions {
    margin-top: 16px;
    display: flex;
    flex-wrap: wrap;
    width: 100%;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
  }

  .json-preview-variants {
    display: flex;
    gap: 6px;
  }

  .json-preview-actions-right {
    display: flex;
    gap: 8px;
    margin-left: auto;
  }

  :global(.cm-editor) {
    outline: none !important;
  }
</style>
