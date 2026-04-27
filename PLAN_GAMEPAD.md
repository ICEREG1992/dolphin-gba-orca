# Piano Gamepad Mobile → PC (Android Only MVP)

## Panoramica

GBA Orca estende le funzionalità di streaming per permettere a giocatori su telefoni Android con controller Bluetooth di controllare i giochi GBA su Dolphin tramite il PC.

## Architettura

- **Client (telefono)**: Gamepad API (`navigator.getGamepads()`) in Chrome Android, polling 60Hz via `requestAnimationFrame`, invio stato JSON su WebSocket.
- **Trasporto**: WebSocket su axum, endpoint `/ws/:slot` (stesso server HTTP porta 8080).
- **Server (PC)**: Riceve stato gamepad, calcola delta vs frame precedente per slot, chiama `SendInput` (Windows API) per simulare tasti tastiera.
- **Requisito PC**: Finestra principale di Dolphin in focus. L'utente configura tasti diversi per ogni slot GBA in Dolphin.

## Mapping tasti per slot

Ogni slot usa un set diverso di tasti. L'utente configura i controlli in Dolphin → GBA → Controls per ogni slot usando questi tasti:

| GBA | Slot 1 | Slot 2 | Slot 3 | Slot 4 |
|-----|--------|--------|--------|--------|
| D-Pad ↑ | UP | W | I | Numpad 8 |
| D-Pad ↓ | DOWN | S | K | Numpad 5 |
| D-Pad ← | LEFT | A | J | Numpad 4 |
| D-Pad → | RIGHT | D | L | Numpad 6 |
| A | Z | E | P | Numpad 1 |
| B | X | Q | O | Numpad 2 |
| L | C | R | U | Numpad 3 |
| R | V | T | Y | Numpad 0 |
| Start | ENTER | F | H | Numpad 9 |
| Select | BACKSPACE | G | N | Numpad . |

Left stick emula D-Pad con deadzone 0.5.

## Protocollo WebSocket

**Direzione**: Client → Server (unidirezionale per MVP)

**Messaggio** (JSON, inviato ogni frame se cambiato):
```json
{
  "axes": [0.0, -0.95, 0.0, 0.0],
  "buttons": [0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]
}
```

- `axes`: 4 assi (left stick X, left stick Y, right stick X, right stick Y), range -1.0 a 1.0
- `buttons`: 17 bottoni (indici standard gamepad: 0=A, 1=B, 2=X, 3=Y, 4=L1, 5=R1, 6=Select, 7=Start, 12=D-pad Up, 13=D-pad Down, 14=D-pad Left, 15=D-pad Right...)

Server confronta con stato precedente per slot, genera `KEYEVENTF_KEYUP` / `0` per ogni delta, chiama `SendInput` una volta per batch.

## File da creare/modificare

### Nuovi file
- `src-tauri/src/input.rs` - Logica input gamepad, mapping VK, SendInput

### File esistenti da modificare
- `src-tauri/src/lib.rs` - Aggiungere modulo input e stato
- `src-tauri/src/http.rs` - Route WebSocket e frontend JS

## MVP Done

> 1-4 giocatori aprono il proprio URL (`/v/1` ... `/v/4`) su Android Chrome con controller Bluetooth accoppiati. Premendo "Connetti controller", il browser rileva il gamepad e mostra "Connesso". Muovendo stick e premendo tasti, il personaggio nel rispettivo gioco GBA sul PC risponde in tempo reale, con la finestra principale di Dolphin in focus sul PC. Nessuna app installata sui telefoni.

## Cosa rimandare post-MVP

- iOS / Safari (Gamepad API instabile)
- Touch fallback
- Configurazione tasti lato client
- Riconnessione WS automatica
- Controller virtuale (ViGEmBus)