<script>
  import { invoke } from "@tauri-apps/api/core";
  import { onMount, onDestroy } from "svelte";

  let windows = [];
  let streams = {};
  let server = { interfaces: [], port: 8080 };
  let selectedIp = "";
  let gbaOnly = true;
  let loading = false;
  let error = "";
  let autoScanInterval = null;

  $: serverUrl = `http://${selectedIp || "..."}:${server.port}`;

  async function loadServerInfo() {
    try {
      server = await invoke("get_server_info");
      if (server.interfaces.length > 0 && !selectedIp) {
        selectedIp = server.interfaces[0].ip;
      }
    } catch (e) {
      console.error("server info:", e);
    }
  }

  async function doScan(silent = false) {
    if (!silent) loading = true;
    if (!silent) error = "";
    try {
      const result = gbaOnly
        ? await invoke("list_gba_windows")
        : await invoke("list_windows");
      windows = result;
      await refreshStreams();
    } catch (e) {
      if (!silent) error = String(e);
    } finally {
      if (!silent) loading = false;
    }
  }

  async function refreshStreams() {
    try {
      const list = await invoke("list_streams");
      const map = {};
      for (const s of list) map[s.slot] = s;
      streams = map;
    } catch (e) {
      console.error(e);
    }
  }

  async function startStream(w) {
    if (!w.gba_slot) return;
    error = "";
    try {
      await invoke("start_stream", { slot: w.gba_slot, windowTitle: w.title });
      await refreshStreams();
    } catch (e) {
      error = String(e);
    }
  }

  async function stopStream(slot) {
    error = "";
    try {
      await invoke("stop_stream", { slot });
      await refreshStreams();
    } catch (e) {
      error = String(e);
    }
  }

  function copyToClipboard(text) {
    navigator.clipboard?.writeText(text);
  }

  onMount(() => {
    loadServerInfo();
    doScan(false);
    autoScanInterval = setInterval(() => doScan(true), 3000);
  });

  onDestroy(() => {
    if (autoScanInterval) clearInterval(autoScanInterval);
  });
</script>

<div class="app">
  <div class="titlebar">
    GBA Orca
  </div>

  <div class="toolbar">
    <button on:click={() => doScan(false)} disabled={loading}>
      {loading ? "Scansione..." : "Aggiorna"}
    </button>
    <label class="chk">
      <input type="checkbox" bind:checked={gbaOnly} />
      Solo finestre GBA
    </label>
    <span class="sep"></span>
    <span class="info">Auto-scan: 3s</span>
  </div>

  <div class="server-row">
    <label>Server:</label>
    {#if server.interfaces.length > 1}
      <select bind:value={selectedIp}>
        {#each server.interfaces as iface}
          <option value={iface.ip}>{iface.ip} — {iface.name}</option>
        {/each}
      </select>
    {/if}
    <code on:click={() => copyToClipboard(serverUrl)} title="Click per copiare">
      {serverUrl}
    </code>
  </div>

  {#if error}
    <div class="error-bar">{error}</div>
  {/if}

  <div class="table-wrap">
    <table>
      <thead>
        <tr>
          <th style="width:50px">Slot</th>
          <th>Titolo finestra</th>
          <th style="width:80px">PID</th>
          <th style="width:90px">Dim.</th>
          <th style="width:280px">Stato</th>
        </tr>
      </thead>
      <tbody>
        {#each windows as w (w.hwnd)}
          <tr class:gba={w.gba_slot}>
            <td class="slot-cell">
              {#if w.gba_slot}<b>GBA{w.gba_slot}</b>{/if}
            </td>
            <td class="title-cell" title={w.title}>{w.title}</td>
            <td class="mono">{w.pid}</td>
            <td class="mono">{w.width}×{w.height}</td>
            <td>
              {#if w.gba_slot}
                {#if streams[w.gba_slot]}
                  {@const url = `${serverUrl}/v/${w.gba_slot}`}
                  <code class="url" on:click={() => copyToClipboard(url)} title="Click per copiare">
                    {url}
                  </code>
                  <button on:click={() => stopStream(w.gba_slot)}>Stop</button>
                {:else}
                  <button on:click={() => startStream(w)}>Avvia stream</button>
                {/if}
              {/if}
            </td>
          </tr>
        {/each}
        {#if windows.length === 0}
          <tr><td colspan="5" class="empty">Nessuna finestra trovata</td></tr>
        {/if}
      </tbody>
    </table>
  </div>

  <div class="statusbar">
    <span>{windows.length} finestre</span>
    <span class="sep-v"></span>
    <span>{Object.keys(streams).length} stream attivi</span>
    <span class="grow"></span>
    <span>{server.interfaces.length} interfacce di rete</span>
  </div>
</div>

<style>
  :global(html), :global(body) {
    margin: 0;
    padding: 0;
    background: #f0f0f0;
    font-family: "Segoe UI", Tahoma, sans-serif;
    font-size: 12px;
    color: #000;
    user-select: none;
  }

  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }

  .titlebar {
    background: linear-gradient(to bottom, #f7f7f7, #e4e4e4);
    border-bottom: 1px solid #b0b0b0;
    padding: 4px 8px;
    font-weight: bold;
    font-size: 13px;
  }

  .toolbar {
    background: #ececec;
    border-bottom: 1px solid #c0c0c0;
    padding: 4px 6px;
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .toolbar .sep {
    width: 1px;
    height: 18px;
    background: #c0c0c0;
    margin: 0 4px;
  }

  .toolbar .info {
    color: #555;
    font-style: italic;
  }

  .chk {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    cursor: pointer;
  }

  .server-row {
    background: #f5f5f5;
    border-bottom: 1px solid #d0d0d0;
    padding: 5px 8px;
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .server-row label {
    font-weight: bold;
  }

  .server-row select {
    font-family: monospace;
    padding: 1px 3px;
    border: 1px solid #888;
  }

  .server-row code {
    font-family: Consolas, monospace;
    background: #fff;
    border: 1px solid #c0c0c0;
    padding: 2px 6px;
    cursor: pointer;
  }

  .server-row code:hover {
    background: #ffffcc;
  }

  .error-bar {
    background: #ffe0e0;
    border-bottom: 1px solid #c00;
    color: #800;
    padding: 4px 8px;
    font-family: monospace;
  }

  .table-wrap {
    flex: 1;
    overflow: auto;
    background: #fff;
    border-top: 1px solid #888;
    border-bottom: 1px solid #888;
  }

  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
  }

  thead {
    position: sticky;
    top: 0;
  }

  th {
    background: linear-gradient(to bottom, #f7f7f7, #e0e0e0);
    border-bottom: 1px solid #888;
    border-right: 1px solid #c0c0c0;
    padding: 3px 6px;
    text-align: left;
    font-weight: normal;
  }

  td {
    border-bottom: 1px solid #eee;
    border-right: 1px solid #f0f0f0;
    padding: 3px 6px;
    vertical-align: middle;
  }

  tbody tr:hover {
    background: #e8f0fe;
  }

  tr.gba {
    background: #fffbe0;
  }

  tr.gba:hover {
    background: #fff5b0;
  }

  .slot-cell {
    text-align: center;
  }

  .title-cell {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 0;
  }

  .mono {
    font-family: Consolas, monospace;
    color: #333;
  }

  .url {
    font-family: Consolas, monospace;
    background: #f0f0f0;
    border: 1px solid #c0c0c0;
    padding: 1px 4px;
    margin-right: 4px;
    cursor: pointer;
    font-size: 11px;
  }

  .url:hover {
    background: #ffffcc;
  }

  .empty {
    text-align: center;
    color: #888;
    padding: 12px;
    font-style: italic;
  }

  /* Bottoni stile Windows classico */
  button {
    background: linear-gradient(to bottom, #f5f5f5, #e0e0e0);
    border: 1px solid #888;
    padding: 2px 10px;
    font-family: "Segoe UI", Tahoma, sans-serif;
    font-size: 12px;
    cursor: pointer;
    min-height: 22px;
  }

  button:hover:not(:disabled) {
    background: linear-gradient(to bottom, #fafafa, #e8e8e8);
    border-color: #5b9ade;
  }

  button:active:not(:disabled) {
    background: linear-gradient(to bottom, #d8d8d8, #ececec);
  }

  button:disabled {
    color: #888;
    cursor: not-allowed;
  }

  .statusbar {
    background: #ececec;
    border-top: 1px solid #c0c0c0;
    padding: 3px 8px;
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 11px;
    color: #444;
  }

  .statusbar .sep-v {
    width: 1px;
    height: 12px;
    background: #b0b0b0;
  }

  .statusbar .grow {
    flex: 1;
  }
</style>