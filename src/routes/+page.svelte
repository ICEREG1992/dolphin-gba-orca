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
    background: #f3f3f3; /* Classico sfondo grigio chiaro Win10 */
    font-family: "Segoe UI", sans-serif;
    font-size: 12px;
    color: #000;
    user-select: none;
  }

  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }

  /* Layout Superiore */
  .titlebar {
    background: #fff;
    border-bottom: 1px solid #e0e0e0;
    padding: 6px 10px;
    font-weight: 600;
    font-size: 13px;
  }

  .toolbar {
    background: #fff;
    border-bottom: 1px solid #e0e0e0;
    padding: 4px 10px;
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .sep {
    width: 1px;
    height: 16px;
    background: #e0e0e0;
  }

  .info {
    color: #666;
    font-style: italic;
  }

  .chk {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    cursor: pointer;
  }

  /* Riga Server */
  .server-row {
    background: #fff;
    border-bottom: 1px solid #e0e0e0;
    padding: 6px 10px;
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .server-row label {
    font-weight: 600;
  }

  /* Campi codice/URL unificati e piatti */
  .server-row select,
  .server-row code,
  .url {
    font-family: "Consolas", monospace;
    background: #fff;
    border: 1px solid #ccc;
    padding: 2px 6px;
    cursor: pointer;
  }

  .server-row code:hover,
  .url:hover {
    border-color: #0078d7; /* Azzurro Win10 */
    background: #e5f1fb;
  }

  /* Barra Errore */
  .error-bar {
    background: #fde7e9;
    border-bottom: 1px solid #c42b1c;
    color: #c42b1c;
    padding: 4px 10px;
    font-family: "Consolas", monospace;
  }

  /* Tabella */
  .table-wrap {
    flex: 1;
    overflow: auto;
    background: #fff;
  }

  table {
    width: 100%;
    border-collapse: collapse;
  }

  th {
    background: #f9f9f9;
    border-bottom: 1px solid #e0e0e0;
    padding: 6px 10px;
    text-align: left;
    font-weight: 600;
  }

  td {
    border-bottom: 1px solid #f0f0f0;
    padding: 5px 10px;
  }

  tbody tr:hover {
    background: #e5f1fb; /* Selezione azzurra Win10 */
  }

  /* GBA ORCA - Più grande e in evidenza */
  tr.gba {
    background: #fff8e1;
    font-size: 15px;      /* Font più grande */
    font-weight: 600;     /* Grassetto */
    padding: 10px;        /* Più spazio interno */
  }

  tr.gba:hover {
    background: #ffecb3;
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
    font-family: "Consolas", monospace;
  }

  .empty {
    text-align: center;
    color: #888;
    padding: 16px;
    font-style: italic;
  }

  /* Bottoni Stile Win10 */
  button {
    background: #fff;
    border: 1px solid #999;
    padding: 4px 12px;
    font-family: "Segoe UI", sans-serif;
    font-size: 12px;
    cursor: pointer;
  }

  button:hover:not(:disabled) {
    background: #e6e6e6;
    border-color: #0078d7;
  }

  button:active:not(:disabled) {
    background: #cccccc;
  }

  button:disabled {
    color: #aaa;
    cursor: not-allowed;
    background: #f5f5f5;
  }

  /* StatusBar */
  .statusbar {
    background: #f3f3f3;
    border-top: 1px solid #e0e0e0;
    padding: 4px 10px;
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 11px;
    color: #555;
  }

  .sep-v {
    width: 1px;
    height: 12px;
    background: #d0d0d0;
  }

  .grow {
    flex: 1;
  }
</style>
