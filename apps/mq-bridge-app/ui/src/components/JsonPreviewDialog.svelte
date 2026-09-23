<script lang="ts">
  import "@awesome.me/webawesome/dist/components/button/button.js";
  import "@awesome.me/webawesome/dist/components/dialog/dialog.js";
  import { tick, onDestroy } from "svelte";
  import { EditorView, basicSetup } from "codemirror";
  import { Compartment } from "@codemirror/state";
  import { json } from "@codemirror/lang-json";
  import { yaml } from "@codemirror/lang-yaml";
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

  let editorContainer = $state<HTMLElement | null>(null);
  let copyLabel = $state("Copy");
  let selectedVariantId = $state<string | null>(null);
  let format = $state<ConfigTextFormat>("json");
  let applyError = $state("");
  let applying = $state(false);
  // Unapplied edits carried across a JSON/YAML switch.
  let draftValue = $state<Record<string, unknown> | undefined>(undefined);
  let editorView: EditorView | null = null;
  let copyTimer: ReturnType<typeof setTimeout> | null = null;
  const languageCompartment = new Compartment();
  const editableCompartment = new Compartment();

  const activeVariant = $derived(
    variants.length > 0 ? (variants.find((variant) => variant.id === selectedVariantId) ?? variants[0]) : null,
  );
  const displayValue = $derived(activeVariant ? activeVariant.value : value);
  const editable = $derived(Boolean(activeVariant?.editable && onApply));

  $effect(() => {
    if (open) {
      void renderText(formatConfigText($state.snapshot(draftValue ?? displayValue), format), format, editable);
    }
  });

  async function renderText(doc: string, docFormat: ConfigTextFormat, isEditable: boolean) {
    await tick();
    if (!editorContainer) return;
    const language = docFormat === "yaml" ? yaml() : json();
    if (!editorView) {
      editorView = new EditorView({
        doc,
        extensions: [
          basicSetup,
          languageCompartment.of(language),
          editableCompartment.of(EditorView.editable.of(isEditable)),
          EditorView.theme({
            "&": { height: "min(64vh, 680px)", fontSize: "13px" },
            ".cm-scroller": { overflow: "auto" },
          }),
        ],
        parent: editorContainer,
      });
      return;
    }
    editorView.dispatch({
      changes: { from: 0, to: editorView.state.doc.length, insert: doc },
      effects: [
        languageCompartment.reconfigure(language),
        editableCompartment.reconfigure(EditorView.editable.of(isEditable)),
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
    if (!onApply || !activeVariant) return;
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

<wa-dialog label={title} open={open} class="json-preview-dialog" onwa-hide={close}>
  <div bind:this={editorContainer} class="json-preview-container"></div>
  {#if applyError}
    <div class="json-preview-error" role="alert">{applyError}</div>
  {/if}
  <div slot="footer" class="json-preview-actions">
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
</wa-dialog>

<style>
  :global(wa-dialog.json-preview-dialog::part(panel)) {
    width: min(920px, calc(100vw - 32px));
  }

  .json-preview-container {
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
    display: flex;
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
