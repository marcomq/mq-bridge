<script lang="ts">
  import "@awesome.me/webawesome/dist/components/details/details.js";
  import {
    activeMainTab,
    consumersPanelState,
    detailPaneTopPercent,
    MAX_DETAIL_PANE_PERCENT,
    MIN_DETAIL_PANE_PERCENT,
    sidebarWidthStore,
  } from "../lib/stores";
  import { getPeerStatus } from "../lib/state/peer-status.svelte";
  import type { ConsumerTreeNode } from "../lib/consumer-grouping";
  import { filterPeerRows, peerConsumerRows } from "../lib/peer-status";
  import PeerSidebarRows from "./PeerSidebarRows.svelte";
  import SidebarImportActions from "./SidebarImportActions.svelte";
  import HeaderRowsEditor from "./HeaderRowsEditor.svelte";
  import JsonPreviewDialog from "./JsonPreviewDialog.svelte";
  import PayloadDisplay from "./PayloadDisplay.svelte";
  import { onMount } from "svelte";
  import {
    addConsumerAction,
    addConsumerResponseHeader,
    clearActiveConsumerHistory,
    cloneCurrentConsumerAction,
    copyCurrentConsumerAction,
    currentConsumerConfigVariants,
    applyCurrentConsumerRawConfig,
    deleteCurrentConsumerAction,
    importAsyncApiToConsumerAction,
    importMqbToConsumerAction,
    restoreConsumerStateFromView,
    selectConsumerSubtab,
    setConsumerMessageCaptureEnabledAction,
    setConsumerMessageCaptureKeepLastAction,
    setConsumerOutputModeAction,
    setConsumerOutputPublisherAction,
    showConsumerMessageDetails,
    toggleConsumerResponseHeader,
    toggleActiveConsumer,
    removeConsumerResponseHeader,
    updateConsumerResponseHeader,
    updateConsumerResponsePayload,
  } from "../lib/consumers-view";
  import { registerDismissOnOutsideClick, startSidebarResize as beginSidebarResize } from "../lib/sidebar-ui";
  import { handleActionKey, getTechnicalDisplayLabel } from "../lib/utils";
  import { formatEndpointTypeLabel, getEndpointTypeGroups } from "../lib/endpoint-metadata";
  import type { ConfigJsonVariant } from "../lib/import-export";
  import { appShell, getAppState } from "../lib/app-shell";

  let filterText = $state("");
  let addMenuOpen = $state(false);
  let consumersContainerEl = $state<HTMLDivElement | null>(null);
  let messagesPaneEl = $state<HTMLDivElement | null>(null);
  let expandedGroupIds = $state<Set<string>>(new Set());
  let knownGroupIds = $state<Set<string>>(new Set());
  let configJsonOpen = $state(false);
  let configJsonVariants = $state<ConfigJsonVariant[]>([]);
  const importActions = [
    { key: "asyncapi", label: "Import AsyncAPI" },
    { key: "mqb", label: "Import mq-bridge" },
  ];

  const selectedConsumer = $derived(
    $consumersPanelState.items.find((item) => item.originalIndex === $consumersPanelState.selectedIndex),
  );
  const selectedConfig = $derived.by(() => {
    const panelState = $consumersPanelState;
    const idx = panelState.selectedIndex;
    if (idx == null || idx < 0) return null;
    return appShell.config()?.consumers?.[idx];
  });

  const selectedSummary = $derived.by(() => {
    if (selectedConfig && selectedConfig.endpoint) {
      return getTechnicalDisplayLabel(selectedConfig.endpoint as Record<string, unknown>, selectedConsumer?.inputProto);
    }
    return "";
  });
  const selectedProto = $derived(selectedConsumer?.inputProto || "");
  const peerRows = $derived(filterPeerRows(peerConsumerRows(getPeerStatus()), filterText));

  async function showCurrentConsumerJson() {
    const variants = await currentConsumerConfigVariants();
    if (!variants) return;
    configJsonVariants = variants;
    configJsonOpen = true;
  }

  type VisibleConsumerTreeRow =
    | { kind: "group"; id: string; label: string; depth: number; expanded: boolean; endpointType?: string; tooltip?: string }
    | {
        kind: "consumer";
        id: string;
        label: string;
        depth: number;
        endpointType: string;
        consumerIndex: number;
        statusClass: string;
        messageCount: number;
        throughputLabel: string;
        tooltip?: string;
      };

  function collectDefaultExpanded(nodes: ConsumerTreeNode[], acc = new Set<string>()) {
    for (const node of nodes) {
      if (node.kind === "group") {
        acc.add(node.id);
        collectDefaultExpanded(node.children, acc);
      }
    }
    return acc;
  }

  function treeNodeMatches(node: ConsumerTreeNode, query: string): boolean {
    const q = query.trim().toLowerCase();
    if (!q) return false;

    if (node.kind === "consumer") {
      return node.label.toLowerCase().includes(q)
        || String(node.tooltip || "").toLowerCase().includes(q)
        || node.endpointType.toLowerCase().includes(q);
    }

    return node.label.toLowerCase().includes(q)
      || String(node.tooltip || "").toLowerCase().includes(q)
      || String(node.endpointType || "").toLowerCase().includes(q)
      || node.children.some((child) => treeNodeMatches(child, query));
  }

  function filterTree(nodes: ConsumerTreeNode[], query: string): ConsumerTreeNode[] {
    const q = query.trim().toLowerCase();
    if (!q) return nodes;

    const result: ConsumerTreeNode[] = [];
    for (const node of nodes) {
      if (node.kind === "consumer") {
        if (treeNodeMatches(node, q)) {
          result.push(node);
        }
        continue;
      }

      const children = filterTree(node.children, q);
      if (children.length > 0 || treeNodeMatches({ ...node, children: [] }, q)) {
        result.push({ ...node, children });
      }
    }
    return result;
  }

  function flattenTree(nodes: ConsumerTreeNode[], depth = 0): VisibleConsumerTreeRow[] {
    const rows: VisibleConsumerTreeRow[] = [];
    const isFiltering = filterText.trim().length > 0;
    for (const node of nodes) {
      if (node.kind === "group") {
        const expanded = expandedGroupIds.has(node.id) || (isFiltering && treeNodeMatches(node, filterText));
        rows.push({
          kind: "group",
          id: node.id,
          label: node.label,
          depth,
          expanded,
          endpointType: node.endpointType,
          tooltip: node.tooltip,
        });
        if (expanded) {
          rows.push(...flattenTree(node.children, depth + 1));
        }
        continue;
      }

      rows.push({
        kind: "consumer",
        id: node.id,
        label: node.label,
        depth,
        endpointType: node.endpointType,
        consumerIndex: node.consumerIndex,
        statusClass: node.statusClass,
        messageCount: node.messageCount,
        throughputLabel: node.throughputLabel,
        tooltip: node.tooltip,
      });
    }
    return rows;
  }

  $effect(() => {
    const defaults = collectDefaultExpanded($consumersPanelState.groupedItems || []);
    const nextKnown = new Set(knownGroupIds);
    const merged = new Set(expandedGroupIds);
    let changed = false;

    for (const groupId of defaults) {
      if (!nextKnown.has(groupId)) {
        nextKnown.add(groupId);
        merged.add(groupId);
        changed = true;
      }
    }

    if (changed) {
      knownGroupIds = nextKnown;
      expandedGroupIds = merged;
    }
  });

  const visibleTreeRows = $derived.by(() =>
    flattenTree(filterTree($consumersPanelState.groupedItems || [], filterText)));

  onMount(() => {
    return registerDismissOnOutsideClick(
      () => addMenuOpen,
      () => {
        addMenuOpen = false;
      },
    );
  });

  function openConsumer(originalIndex: number) {
    getAppState().last_consumer_idx = originalIndex;
    window.history.replaceState(null, "", `#consumers:${originalIndex}`);
    restoreConsumerStateFromView(originalIndex);
  }

  function toggleGroup(groupId: string) {
    const next = new Set(expandedGroupIds);
    if (next.has(groupId)) {
      next.delete(groupId);
    } else {
      next.add(groupId);
    }
    expandedGroupIds = next;
  }

  function handleAdd(type: string) {
    void addConsumerAction(type);
    addMenuOpen = false;
  }

  function setMessageCaptureEnabled(event: Event) {
    setConsumerMessageCaptureEnabledAction((event.currentTarget as HTMLInputElement).checked);
  }

  function setMessageCaptureKeepLast(event: Event) {
    setConsumerMessageCaptureKeepLastAction(Number((event.currentTarget as HTMLSelectElement).value));
  }

  async function copyMessageDetails() {
    if (!$consumersPanelState.detailRequestPayload) return;
    await navigator.clipboard.writeText($consumersPanelState.detailRequestPayload);
  }

  function setOutputMode(event: Event) {
    setConsumerOutputModeAction((event.currentTarget as HTMLSelectElement).value as "none" | "publisher" | "response");
  }

  function setOutputPublisher(event: Event) {
    setConsumerOutputPublisherAction((event.currentTarget as HTMLSelectElement).value);
  }

  async function handleImport(actionKey: string, text: string) {
    try {
      if (actionKey === "asyncapi") {
        await importAsyncApiToConsumerAction(text);
      } else {
        await importMqbToConsumerAction(text);
      }
    } catch (error) {
      console.error(error);
    }
  }

  function formatThroughput(label: string) {
    if (!label) return "";
    return label.replace(/(\d)(?=(\d{3})+(?!\d))/g, "$1\u2009");
  }

  function startSidebarResize(event: MouseEvent) {
    beginSidebarResize(event, consumersContainerEl, (nextWidth) => {
      sidebarWidthStore.set(nextWidth);
    });
  }

  function startMessagePaneResize(event: MouseEvent) {
    if (!messagesPaneEl) return;
    event.preventDefault();
    const container = messagesPaneEl;

    const updateSplit = (clientY: number) => {
      const rect = container.getBoundingClientRect();
      const offset = clientY - rect.top;
      const clampedOffset = Math.min(Math.max(offset, 100), Math.max(rect.height - 100, 100));
      detailPaneTopPercent.set(
        Math.min(Math.max((clampedOffset / rect.height) * 100, MIN_DETAIL_PANE_PERCENT), MAX_DETAIL_PANE_PERCENT),
      );
    };

    updateSplit(event.clientY);

    const onMove = (moveEvent: MouseEvent) => updateSplit(moveEvent.clientY);
    const onUp = () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };

    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  }
</script>

<div
  class:active={$activeMainTab === "consumers"}
  class="tab-content-panel"
  id="tab-consumers"
  style="width: 100%"
>
  <div bind:this={consumersContainerEl} id="consumers-container" class="publishers-layout">
    <div class="sidebar" style={`width:${$sidebarWidthStore}px;`}>
      <div class="sidebar-header">
        <input class="sidebar-search" id="cons-filter" type="text" placeholder="Filter consumers…" bind:value={filterText} />
        <div class="add-menu-container">
          <wa-button
            appearance="outlined"
            class="icon-button"
            id="cons-add"
            title="Add consumer"
            role="button"
            tabindex="0"
            onclick={() => (addMenuOpen = !addMenuOpen)}
            onkeydown={(event: KeyboardEvent) => handleActionKey(event, () => (addMenuOpen = !addMenuOpen))}
            >+</wa-button>
          {#if addMenuOpen}
            <div class="add-menu">
              {#each getEndpointTypeGroups("consumer") as group (group.label)}
                {#if group.label}
                  <div class="add-menu-label">{group.label}</div>
                {/if}
                {#each group.kinds as type (type)}
                  <button type="button" onclick={() => handleAdd(type)}>{formatEndpointTypeLabel(type)}</button>
                {/each}
              {/each}
            </div>
          {/if}
        </div>
      </div>
      <div class="sidebar-list" id="cons-list">
        <div class="sidebar-group-label">Receive</div>
        {#each visibleTreeRows as row, i (row.id + '-' + i)}
          {#if row.kind === "group"}
            <button
              type="button"
              class="sidebar-item sidebar-item--group"
              title={row.tooltip}
              onclick={() => toggleGroup(row.id)}
              onkeydown={(event: KeyboardEvent) => handleActionKey(event, () => toggleGroup(row.id))}
            >
              <span class="tree-guide" aria-hidden="true" style={`width:${row.depth * 14}px;`}></span>
              {#if row.depth === 0 && row.endpointType}
                <span class={`proto-badge proto-${row.endpointType.toLowerCase()}`}>{row.endpointType}</span>
              {/if}
              <span class="tree-caret">{row.expanded ? "▾" : "▸"}</span>
              <span class="item-name">{row.label}</span>
            </button>
          {:else}
            <button
              type="button"
              class:active={$consumersPanelState.selectedIndex === row.consumerIndex}
              class="sidebar-item cons-item"
              data-idx={row.consumerIndex}
              title={row.tooltip}
              onclick={() => openConsumer(row.consumerIndex)}
            >
              <span class="tree-guide" aria-hidden="true" style={`width:${row.depth * 14}px;`}></span>
              {#if row.depth === 0}
                <span class={`proto-badge proto-${row.endpointType.toLowerCase()}`}>{row.endpointType}</span>
              {/if}
              <span class="item-name">{row.label}</span>
              <span class="msg-count" style="margin-left:auto;" title={`${row.messageCount} total messages`}>
                {formatThroughput(row.throughputLabel)}
              </span>
              <span class={`item-status ${row.statusClass}`}></span>
            </button>
          {/if}
        {/each}
        <PeerSidebarRows rows={peerRows} itemClass="cons-item" />
      </div>
      <SidebarImportActions actions={importActions} onImport={handleImport} />
    </div>
    <button
      type="button"
      class="sidebar-resizer"
      aria-label="Resize consumer sidebar"
      tabindex="-1"
      onmousedown={startSidebarResize}
    ></button>
    <div class="main-content">
      <div id="cons-empty-alert" class="empty-state" style:display={$consumersPanelState.hasConsumers ? "none" : "block"}>
        No consumers configured. Click "+" to create one.
      </div>
      <div id="cons-main-ui" style:display={$consumersPanelState.hasConsumers ? "contents" : "none"}>
        <div class="request-bar">
          <div class="request-field request-field--wide">
            <input class="url-input request-field-input" readonly value={selectedSummary} />
          </div>
          <div class="request-field" style="align-items: center; justify-content: flex-start; gap: 12px; margin-left: 12px;">
            <wa-badge
              appearance="filled-outlined"
              class="consumer-live-badge"
              class:consumer-live-badge--success={$consumersPanelState.liveStatusVariant === "success"}
              class:consumer-live-badge--danger={$consumersPanelState.liveStatusVariant === "danger"}
              class:consumer-live-badge--neutral={$consumersPanelState.liveStatusVariant === "neutral"}
              variant={$consumersPanelState.liveStatusVariant}
            >
              {$consumersPanelState.liveStatusText}
            </wa-badge>
          </div>
          <div class="consumer-message-controls">
            <label class="consumer-message-control consumer-message-control--checkbox" for="cons-capture-enabled">
              <input
                id="cons-capture-enabled"
                type="checkbox"
                checked={$consumersPanelState.messageCaptureEnabled}
                onchange={setMessageCaptureEnabled}
              />
              <span>Capture messages</span>
            </label>
            <label class="consumer-message-control" for="cons-capture-keep-last">
              <span>Keep last</span>
              <select
                id="cons-capture-keep-last"
                class="field-input consumer-message-select"
                value={String($consumersPanelState.messageCaptureKeepLast)}
                onchange={setMessageCaptureKeepLast}
              >
                <option value="10">10</option>
                <option value="100">100</option>
                <option value="500">500</option>
              </select>
            </label>
          </div>
          <wa-button
            variant="neutral"
            appearance="outlined"
            class="ghost-action"
            id="cons-clear-history"
            role="button"
            tabindex="0"
            onclick={clearActiveConsumerHistory}
            onkeydown={(event: KeyboardEvent) => handleActionKey(event, clearActiveConsumerHistory)}
            >Clear</wa-button
          >
          <wa-button
            variant={$consumersPanelState.toggleVariant}
            id="cons-toggle"
            loading={$consumersPanelState.toggleBusy}
            disabled={$consumersPanelState.toggleBusy}
            role="button"
            tabindex={$consumersPanelState.toggleBusy ? "-1" : "0"}
            onclick={toggleActiveConsumer}
            onkeydown={(event: KeyboardEvent) => handleActionKey(event, toggleActiveConsumer)}
          >
            {$consumersPanelState.toggleLabel}
          </wa-button>
        </div>
        <div class="content-tabs" id="cons-sub-tabs">
          <button
            type="button"
            class:active={$consumersPanelState.activeSubtab === "definition"}
            class="content-tab"
            data-target="cons-def-panel"
            id="ctab-def"
            onclick={() => selectConsumerSubtab("definition")}>Definition</button
          >
          <button
            type="button"
            class:active={$consumersPanelState.activeSubtab === "response"}
            class="content-tab"
            data-target="cons-response-panel"
            id="cons-response-tab"
            onclick={() => selectConsumerSubtab("response")}
          >
            Output
          </button>
          <button
            type="button"
            class:active={$consumersPanelState.activeSubtab === "messages"}
            class="content-tab"
            data-target="cons-msg-panel"
            id="ctab-msg"
            onclick={() => selectConsumerSubtab("messages")}>Messages</button
          >
        </div>
        <div
          class="pane-container"
          id="cons-def-panel"
          style:display={$consumersPanelState.activeSubtab === "definition" ? "flex" : "none"}
        >
          <div class="pane-top pane-top-form" style="padding:12px;">
            <div class="section-toolbar editor-action-bar editor-action-bar--compact">
              <div class="form-actions-row section-actions section-actions-right">
                <div class="editor-action-cluster">
                  <wa-button
                    variant="neutral"
                    appearance="outlined"
                    id="cons-copy"
                    role="button"
                    tabindex="0"
                    onclick={copyCurrentConsumerAction}
                    onkeydown={(event: KeyboardEvent) => handleActionKey(event, copyCurrentConsumerAction)}
                    >Copy to Publisher</wa-button
                  >
                  <wa-button
                    variant="neutral"
                    appearance="outlined"
                    id="cons-save-variant"
                    role="button"
                    tabindex="0"
                    onclick={cloneCurrentConsumerAction}
                    onkeydown={(event: KeyboardEvent) => handleActionKey(event, cloneCurrentConsumerAction)}
                    >Clone</wa-button
                  >
                  <wa-button
                    variant="neutral"
                    appearance="outlined"
                    id="cons-export-config"
                    title="Show consumer JSON"
                    aria-label="Show consumer JSON"
                    role="button"
                    tabindex="0"
                    onclick={() => void showCurrentConsumerJson()}
                    onkeydown={(event: KeyboardEvent) => handleActionKey(event, () => void showCurrentConsumerJson())}
                    >{"{}"}</wa-button
                  >
                </div>
                <div class="toolbar-divider" aria-hidden="true"></div>
                <div class="editor-action-cluster">
                  <wa-button
                    variant="danger"
                    appearance="outlined"
                    id="cons-delete"
                    role="button"
                    tabindex="0"
                    onclick={deleteCurrentConsumerAction}
                    onkeydown={(event: KeyboardEvent) => handleActionKey(event, deleteCurrentConsumerAction)}
                    >Delete</wa-button
                  >
                </div>
              </div>
            </div>
            <div class="form-scroll-wrapper">
              <div class="section-label">Definition</div>
              <div id="cons-config-form" class="field-grid"></div>
            </div>
          </div>
        </div>
        <div
          class="pane-container"
          id="cons-response-panel"
          style:display={$consumersPanelState.activeSubtab === "response" ? "flex" : "none"}
        >
          <div class="pane-top" style="padding:12px;">
            <div id="cons-response-editor">
              <div class="section-toolbar response-editor-header">
                <div class="section-label">Output</div>
                <span class="form-description">
                  Choose whether this consumer should acknowledge, forward to a publisher, or return a custom response.
                </span>
              </div>
              <div class="response-editor-grid">
                <div class="wa-form-row field-grid">
                  <label class="wa-form-label" for="cons-output-mode">Mode</label>
                  <div class="wa-form-col">
                    <select
                      id="cons-output-mode"
                      class="field-input"
                      value={$consumersPanelState.outputMode}
                      onchange={setOutputMode}
                    >
                      <option value="none">None</option>
                      <option value="publisher">Publisher</option>
                      <option value="response" disabled={!$consumersPanelState.responseSupported}>Response</option>
                    </select>
                  </div>
                </div>
              </div>
              {#if $consumersPanelState.outputMode === "publisher"}
                <div class="response-editor-grid">
                  <div class="wa-form-row field-grid">
                    <label class="wa-form-label" for="cons-output-publisher">Publisher</label>
                    <div class="wa-form-col">
                      <select
                        id="cons-output-publisher"
                        class="field-input"
                        value={$consumersPanelState.selectedPublisher}
                        onchange={setOutputPublisher}
                      >
                        <option value="">Select a publisher…</option>
                        {#each $consumersPanelState.publisherOptions as publisher (publisher.value)}
                          <option value={publisher.value}>{publisher.label}</option>
                        {/each}
                      </select>
                    </div>
                  </div>
                </div>
              {/if}
              {#if $consumersPanelState.outputMode === "response"}
              <div class="response-editor-grid">
                <div class="section-label">Headers</div>
                <div id="cons-response-headers">
                  <HeaderRowsEditor
                    rows={$consumersPanelState.responseHeaders}
                    addLabel="Add Header"
                    showEnabled={true}
                    onAdd={addConsumerResponseHeader}
                    onUpdate={updateConsumerResponseHeader}
                    onToggle={toggleConsumerResponseHeader}
                    onRemove={removeConsumerResponseHeader}
                  />
                </div>
                <PayloadDisplay
                  id="cons-response-payload"
                  label="Payload"
                  payload={$consumersPanelState.responsePayload}
                  placeholder="Response body"
                  contentType={$consumersPanelState.responseContentType || ''}
                  readOnly={false}
                  onChange={updateConsumerResponsePayload}
                />
              </div>
              {/if}
            </div>
          </div>
        </div>
        <div
          bind:this={messagesPaneEl}
          class="pane-container"
          id="cons-msg-panel"
          style:display={$consumersPanelState.activeSubtab === "messages" ? "flex" : "none"}
        >
          <div
            class="pane-top"
            id="cons-list-pane"
            style={`flex: 0 0 calc(${$detailPaneTopPercent}% - 1.5px);`}
          >
            <div style="overflow:auto;flex:1;" id="consumer-log-body-wrapper">
              <table class="msg-table">
                <thead>
                  <tr>
                    <th style="width: 170px;">Time</th>
                    <th>Payload Preview</th>
                  </tr>
                </thead>
                <tbody id="consumer-log-body">
                  {#if $consumersPanelState.messages.length === 0}
                    <tr>
                      <td colspan="2" style="text-align:center;padding:20px;color:var(--text-dim);">
                        Waiting for messages...
                      </td>
                    </tr>
                  {:else}
                    {#each $consumersPanelState.messages as message (message.messageIndex)}
                      <tr
                        class:selected={message.selected}
                        style="cursor:pointer;"
                        tabindex="0"
                        onclick={() => {
                          if ($consumersPanelState.currentConsumerKey) {
                            showConsumerMessageDetails($consumersPanelState.currentConsumerKey, message.messageIndex);
                          } else {
                            console.warn("Cannot show message details: currentConsumerKey is not set.");
                          }
                        }}
                        onkeydown={(event: KeyboardEvent) => handleActionKey(event, () => {
                          if ($consumersPanelState.currentConsumerKey) {
                            showConsumerMessageDetails($consumersPanelState.currentConsumerKey, message.messageIndex);
                          }
                        })}
                      >
                        <td class="text-muted small">{message.timeLabel}</td>
                        <td class="font-monospace small text-break text-truncate" style="max-width: 400px;">
                          {message.payloadPreview}
                        </td>
                      </tr>
                    {/each}
                  {/if}
                </tbody>
              </table>
            </div>
          </div>
          <button
            type="button"
            class="pane-divider"
            id="cons-pane-divider"
            aria-label="Resize consumer message details"
            style="height: 3px;"
            onmousedown={startMessagePaneResize}
          ></button>
          <div
            class="pane-bottom"
            id="cons-detail-pane"
            style={`height: auto; flex: 1 1 calc(${100 - $detailPaneTopPercent}% - 1.5px);`}
          >
            <div class="detail-header">
              <span id="cons-msg-detail-info" style="display:block;width:52%;">{$consumersPanelState.detailInfo}</span>
              <span
                style="margin-left:auto;cursor:pointer;color:var(--text-dim)"
                id="cons-msg-copy-btn"
                role="button"
                tabindex="0"
                onclick={copyMessageDetails}
                onkeydown={(event: KeyboardEvent) => handleActionKey(event, copyMessageDetails)}
                >⊕ copy</span
              >
            </div>
            <div class="detail-body" id="cons-msg-details-content">
            {#if $consumersPanelState.detailRequestHeaders.length > 0}
              <wa-details summary="Message Headers" open class="response-meta-block" icon-placement="start">
                <span slot="expand-icon">▸</span>
                <span slot="collapse-icon">▸</span>
                {#each $consumersPanelState.detailRequestHeaders as [key, value], i (`${key}:${value}-${i}`)}
                  <div class="response-meta-row">
                    <span class="response-meta-key">{key}</span>
                    <span class="response-meta-value">{value}</span>
                  </div>
                {/each}
              </wa-details>
            {/if}
              <PayloadDisplay
                id="cons-msg-payload"
                label="Message Body"
                payload={$consumersPanelState.detailRequestPayload}
                contentType={$consumersPanelState.detailRequestContentType || ''}
                readOnly={true}
                showPretty={true}
              />
            {#if $consumersPanelState.hasResponse}
              <wa-details summary="Response Headers" open class="response-meta-block" icon-placement="start" style="margin-top:16px;">
                <span slot="expand-icon">▸</span>
                <span slot="collapse-icon">▸</span>
                {#each $consumersPanelState.detailResponseHeaders as [key, value], i (`resp:${key}:${value}-${i}`)}
                  <div class="response-meta-row">
                    <span class="response-meta-key">{key}</span>
                    <span class="response-meta-value">{value}</span>
                  </div>
                {:else}
                  <div class="text-muted small" style="padding: 4px 8px;">No headers sent.</div>
                {/each}
              </wa-details>
                <PayloadDisplay
                  id="cons-msg-response"
                  label="Response Body"
                  payload={$consumersPanelState.detailResponsePayload}
                  contentType={$consumersPanelState.detailResponseContentType || ""}
                  readOnly={true}
                  showPretty={true}
                />
              {/if}
            </div>
          </div>
        </div>
      </div>
    </div>
  </div>
</div>

<JsonPreviewDialog
  open={configJsonOpen}
  title="Consumer Configuration"
  variants={configJsonVariants}
  onClose={() => (configJsonOpen = false)}
  onApply={(_variantId, value) => applyCurrentConsumerRawConfig(value)}
/>

<style>
  .consumer-message-controls {
    display: inline-flex;
    align-items: center;
    gap: 12px;
    margin-left: auto;
    margin-right: 8px;
  }

  .consumer-message-control {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    color: var(--text-dim);
    font-size: 12px;
  }

  .consumer-message-control--checkbox input {
    margin: 0;
  }

  .consumer-message-select {
    min-width: 72px;
    padding: 4px 8px;
    min-height: 30px;
  }

  :global(.throughput-sep) {
    opacity: 0.4;
    font-size: 0.9em;
  }

  .hidden-file-input {
    display: none;
  }
</style>
