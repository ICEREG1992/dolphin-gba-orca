# GBA Orca

Mobile streaming solution for Dolphin's internal GBA emulator.

GBA Orca scopre automaticamente le finestre GBA aperte da Dolphin (modalità GBA Player o connessione Game Boy Advance — Mario Party, Final Fantasy: Crystal Chronicles, The Legend of Zelda: Four Swords Adventures, Pokémon Colosseum) e le streamma in tempo reale ai dispositivi connessi alla stessa rete locale. Ogni giocatore vede solo la propria finestra GBA sul telefono o tablet, mentre il TV/monitor principale mostra il GameCube.

Niente cavi, niente Game Boy fisici, niente capture card. Funziona da una qualsiasi LAN/Wi-Fi domestica.

## Come funziona

```
[Dolphin]  ──finestre GBA1..GBA4──▶  [GBA Orca su Windows]
                                            │
                                            ├── FFmpeg cattura ogni finestra
                                            ├── Server HTTP integrato (porta 8080)
                                            └── Stream MJPEG via LAN
                                                    │
                                                    ▼
                                     [Telefoni / tablet sulla stessa rete]
                                       http://192.168.x.x:8080/v/1
                                       http://192.168.x.x:8080/v/2
                                       ...
```

L'app desktop fa da:
- **scanner**: enumera in tempo reale le finestre Windows e isola quelle GBA1..GBA4
- **process manager**: lancia un processo FFmpeg per ogni finestra streammata, lo termina al click di Stop e lo rimuove automaticamente se crasha
- **proxy HTTP**: un singolo server axum su porta 8080 serve sia la pagina viewer (`/v/<slot>`) che il flusso MJPEG (`/stream/<slot>`), supportando viewer multipli per stream
- **discovery**: rileva tutte le interfacce di rete del PC e propone la più probabile interfaccia LAN, con fallback a un dropdown se ne sono presenti più di una

I dispositivi mobili non hanno bisogno di nessuna app installata: aprono direttamente l'URL nel browser.

## Requisiti

- Windows 10 o 11
- Dolphin Emulator con un gioco che apre finestre GBA
- Una rete Wi-Fi/LAN condivisa tra PC e dispositivi mobili
- Per gli utenti finali: scarica direttamente l'installer dalla sezione [Releases](https://github.com/regitkin/dolphin-gba-orca/releases)
- Per gli sviluppatori: Rust toolchain, Node.js 18+, Microsoft C++ Build Tools

## Uso (utenti finali)

1. Installa GBA Orca dall'ultimo installer in [Releases](https://github.com/regitkin/dolphin-gba-orca/releases)
2. Avvia Dolphin e fai partire un gioco multi-GBA — le 1-4 finestre GBA appariranno automaticamente
3. Apri GBA Orca: la lista mostra le finestre rilevate, evidenziate in giallo
4. Per ognuna, click su **Avvia stream**
5. La barra in alto mostra l'URL LAN del server (es. `http://192.168.1.42:8080`). Click sull'URL per copiarlo.
6. Sul telefono di ogni giocatore, apri il browser su `http://192.168.1.42:8080/v/1` (sostituendo `1` con lo slot del giocatore)
7. Il bottone tondo in basso a destra del viewer ruota il video di 90° per giocare comodamente in landscape

L'app fa rescan automatico ogni 3 secondi: se chiudi/apri/riavvii Dolphin durante una sessione, le finestre vengono aggiornate da sole.

## Build da sorgenti

```bash
git clone https://github.com/regitkin/dolphin-gba-orca.git
cd dolphin-gba-orca
npm install
```

Scarica un binario di FFmpeg per Windows (build essentials di [gyan.dev](https://www.gyan.dev/ffmpeg/builds/)), estrai `ffmpeg.exe` e copialo in `src-tauri/binaries/` rinominandolo come:

```
src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe
```

Il triple `x86_64-pc-windows-msvc` è richiesto da Tauri per il pattern sidecar. Verifica il tuo target con `rustc -Vv` (riga `host:`).

Poi:

```bash
# Sviluppo (hot reload)
npm run tauri dev

# Build di produzione
npm run tauri build
```

L'installer MSI/NSIS finale viene generato in `src-tauri/target/release/bundle/`.

## Stack tecnico

| Componente | Tecnologia |
|---|---|
| Desktop shell | [Tauri 2](https://tauri.app/) |
| Backend | Rust |
| Frontend desktop | [Svelte](https://svelte.dev/) (UI stile Windows nativo) |
| Cattura schermo | FFmpeg con `gdigrab` + `mpdecimate` |
| Codec stream | MJPEG su `multipart/x-mixed-replace` |
| Server HTTP | [axum](https://github.com/tokio-rs/axum) + [tokio](https://tokio.rs/) |
| Enum finestre Win32 | crate [`windows`](https://crates.io/crates/windows) (binding ufficiali Microsoft) |
| Discovery interfacce | crate [`local-ip-address`](https://crates.io/crates/local-ip-address) |

### Dettagli tecnici

- **Cattura senza re-encoding pesante**: `gdigrab` legge i pixel direttamente dalla GDI di Windows, `mpdecimate` scarta i frame quasi identici al precedente, `mjpeg` produce un flusso semplicissimo da decodificare per qualsiasi browser. Il risultato: bassa CPU quando la finestra è statica, framerate pieno quando c'è movimento.
- **Pipeline FFmpeg → axum**: ogni FFmpeg pubblica su una porta TCP locale (9001-9004), un task tokio in axum legge i byte, mantiene l'ultimo frame come "primer" per i nuovi client, e li broadcasta a tutti i viewer subscribed via canale `tokio::sync::broadcast`.
- **Multi-viewer per stream**: a differenza dell'HTTP server interno di FFmpeg (che chiude il listener al primo client), il proxy axum permette a quanti dispositivi vuoi di guardare lo stesso slot.
- **Cleanup automatico**: se FFmpeg muore (finestra chiusa, fullscreen non catturabile, ecc.), il task observer rimuove la sessione dallo state e l'UI ritorna allo stato "non in stream" entro il prossimo auto-scan.

## Limitazioni note

- **Solo Windows**: la cattura usa `gdigrab` (GDI Win32). Porting a macOS/Linux richiederebbe sostituire il backend FFmpeg con `avfoundation` o `x11grab` e ri-scrivere il modulo enum finestre.
- **Finestra deve essere visibile**: una finestra GBA minimizzata o coperta da fullscreen non viene catturata. L'app filtra le finestre `0x0` dall'enumerazione e mostra un errore se gdigrab fallisce.
- **Latenza ~150-300ms**: accettabile per giochi GBA in coop ma non per gameplay competitivo. Per latenza inferiore servirebbe WebRTC (roadmap futura, probabilmente integrando MediaMTX).
- **Streaming non cifrato**: il flusso HTTP è in chiaro sulla LAN. Pensato per uso domestico privato.

## Roadmap

- [ ] Routing PIN: ogni slot riceve un codice 4-cifre, i telefoni inseriscono il PIN invece dell'URL completo
- [ ] WebRTC via MediaMTX per latenza sotto i 100ms
- [ ] Refactor del backend in moduli separati (`windows.rs`, `streams.rs`, `server.rs`, `network.rs`)
- [ ] Build macOS/Linux

## Licenza

[Da definire — aggiungi MIT o GPL nel repo se vuoi]

## Contributi

Issue e pull request benvenuti. Il progetto è in fase iniziale, l'architettura interna è ancora in evoluzione.
