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

<main>
  <header>
    <h1>GBA Orca</h1>
    <div class="server-banner">
  <span class="label">Server LAN:</span>
  {#if server.interfaces.length > 1}
    <select bind:value={selectedIp} class="iface-select">
      {#each server.interfaces as iface}
        <option value={iface.ip}>
          {iface.ip} — {iface.name}
        </option>
      {/each}
    </select>
  {/if}
  <code on:click={() => copyToClipboard(serverUrl)} title="Click per copiare">
    {serverUrl}
  </code>
</div>
  </header>

  <div class="controls">
    <button on:click={() => doScan(false)} disabled={loading}>
      {loading ? "Scansione..." : "Scansiona ora"}
    </button>
    <label>
      <input type="checkbox" bind:checked={gbaOnly} />
      Solo finestre GBA
    </label>
    <span class="auto-tag">auto-scan ogni 3s</span>
  </div>

  {#if error}
    <div class="error">{error}</div>
  {/if}

  <p class="count">{windows.length} finestre {gbaOnly ? "GBA" : "totali"}</p>

  <ul class="window-list">
    {#each windows as w (w.hwnd)}
      <li class:gba={w.gba_slot}>
        {#if w.gba_slot}
          <span class="badge">GBA{w.gba_slot}</span>
        {/if}
        <span class="title">{w.title}</span>
        <span class="meta">PID {w.pid} · {w.width}×{w.height}</span>
        {#if w.gba_slot}
          {#if streams[w.gba_slot]}
            {@const url = `${serverUrl}/v/${w.gba_slot}`}
            <div class="stream-live">
              <code on:click={() => copyToClipboard(url)} title="Click per copiare">
                {url}
              </code>
              <button on:click={() => stopStream(w.gba_slot)}>Stop</button>
            </div>
          {:else}
            <button on:click={() => startStream(w)}>Start stream</button>
          {/if}
        {/if}
      </li>
    {/each}
  </ul>
</main>

<style>
  main { font-family: system-ui, sans-serif; padding: 1.5rem; max-width: 1100px; margin: 0 auto; }
  header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 1rem; flex-wrap: wrap; gap: 1rem; }
  h1 { margin: 0; }
  .server-banner { display: flex; align-items: center; gap: 0.6rem; background: #1a2a1a; border: 1px solid #00c864; padding: 0.5rem 0.8rem; border-radius: 6px; }
  .server-banner .label { color: #888; font-size: 0.85rem; }
  .server-banner code { color: #00c864; font-size: 0.95rem; cursor: pointer; user-select: all; }
  .controls { display: flex; gap: 1rem; align-items: center; margin-bottom: 1rem; }
  button { padding: 0.4rem 0.9rem; cursor: pointer; }
  .auto-tag { color: #666; font-size: 0.8rem; font-style: italic; }
  .count { color: #888; font-size: 0.9rem; }
  .error { background: #5a1a1a; color: #ffc; padding: 0.6rem 1rem; border-radius: 4px; margin-bottom: 1rem; }
  .window-list { list-style: none; padding: 0; margin: 0; }
  .window-list li {
    padding: 0.6rem 0.8rem;
    border-bottom: 1px solid #2a2a2a;
    display: grid;
    grid-template-columns: auto 1fr auto auto;
    gap: 0.8rem;
    align-items: center;
  }
  .window-list li.gba { background: rgba(0, 200, 100, 0.08); }
  .badge { background: #00c864; color: black; font-weight: bold; padding: 0.15rem 0.5rem; border-radius: 4px; font-size: 0.8rem; }
  .title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .meta { color: #888; font-size: 0.8rem; font-family: monospace; }
  .stream-live { display: flex; gap: 0.5rem; align-items: center; }
  .stream-live code { color: #00c864; font-family: monospace; font-size: 0.85rem; cursor: pointer; user-select: all; background: #0a1a0a; padding: 0.2rem 0.5rem; border-radius: 3px; }
</style>