# Ironpaw

Ironpaw is a personal AI agent that runs on your own machine and can hold a NEAR wallet.
You talk to it from a web UI, a terminal, Telegram, or a small ESP32 voice device. The
wallet is an [OutLayer](https://app.outlayer.ai/docs/getting-started) custody wallet: the
model never sees its API key, and a request that moves money waits for your click.

This branch (`hackathon`) is the NEAR HACKATHON 2026 submission.

## The problem

An agent that reads email and web pages can be talked into things by the text it reads
(prompt injection). If that agent holds a wallet key, a successful injection is a payment.
Ironpaw assumes the model can be fooled and puts the two things that matter outside its
reach:

- **The key.** The OutLayer API key is stored encrypted on the host. The host adds it as a
  bearer header at the last step, and only for hosts the tool was granted at install time.
  It never enters the model's context or the WASM tool's memory.
- **The decision.** The host holds every state-changing wallet request and asks you with
  Approve / Reject buttons. The approval is bound to a fingerprint of the exact request
  (wallet, method, URL, body), works once and expires after 10 minutes. The button press
  reaches the host as a user message, which the model cannot produce.

## Try it

You need Rust, Node.js 20 or newer, and a
[NEAR AI Cloud API key](https://docs.near.ai/cloud/quickstart.md). These steps use NEAR
testnet through the real OutLayer testnet API, so no funds are needed.

```sh
git clone <this repo> && cd ironpaw && git checkout hackathon
# Make the OutLayer tool installable (the prebuilt .wasm is in the repo).
mkdir -p target/debug/tool-store/outlayer
cp wasm-tools/outlayer/outlayer.wasm wasm-tools/outlayer/outlayer.*.json target/debug/tool-store/outlayer/
./run.sh        # backend on 127.0.0.1:7878, web UI on http://127.0.0.1:3000
```

| Do | See |
|---|---|
| Copy the `local client connection token` that `run.sh` prints, open http://127.0.0.1:3000, paste it and press **Open Ironpaw**. | The chat. On the first run Ironpaw shows a 6-word unlock phrase, checks that you wrote it down, and asks for the NEAR AI key. |
| Say `Create an OutLayer wallet for me.` | The host (not the model) asks to install the tool. Press **Install**, type a name, pick **Testnet**, **Create new**, **Finish**. You get the wallet's NEAR account id; the API key is never shown. |
| Say `Fund the wallet from the faucet and check its balance.` | About 5 testnet NEAR, with no approval asked: reads and the faucet are on the safe list. |
| Say `Send 1 NEAR to <account>.testnet`. | An approval card with the amount and the destination; open it for the full request. Nothing has been sent yet. Press **Approve** and the transfer goes out. |
| Ask for another transfer and press **Reject**. | The agent reports that nothing was sent. The balance confirms it. |

The same flow from the ESP32 board is described in
[local-clients/ironpaw-esp32](local-clients/ironpaw-esp32/README.md); without a board,
its simulator (`local-clients/ironpaw-esp32/sim`) runs the same app code on the PC.

## NEAR features used

| Feature | Where |
|---|---|
| OutLayer agent custody (keys held in a TEE; register, balance, transfer, contract call, withdraw) | `wasm-tools/outlayer`, `src/setup/tool` |
| NEAR Intents swaps through OutLayer (`/wallet/v1/intents/*`, mainnet only) | `wasm-tools/outlayer/src/lib.rs` |
| NEAR AI Cloud inference: chat, vision, image generation, embeddings, Whisper speech to text | `src/inference.rs`, `src/config.rs` |
| NEAR testnet faucet, limited to the wallet's own account | `wasm-tools/outlayer/src/lib.rs` |

## How a payment is handled

1. The model calls the `outlayer` tool with `method`, `path`, `body` and a wallet name.
2. The WASM guest validates the request (a path cannot change the host; `/register` is
   refused, wallets are only created by the host's setup flow) and calls the host's
   `http_request`.
3. The host checks the approval rule (`require_approval` in `src/tools/wasm.rs`). `GET`
   requests and the paths in `approval.safe_paths` (quotes, dry runs, storage deposit,
   faucet) pass. Everything else is stored as pending and the tool returns
   `APPROVAL_REQUIRED`.
4. You press Approve. The model repeats the identical call once; the fingerprint matches
   and the approval is consumed. Any changed field asks again.
5. The host decrypts the key and adds `Authorization: Bearer ...` only for a granted
   host (`src/network/http.rs`).

The approval rules have unit tests (single use, reject, restart, one pending question,
per-client rule):

```sh
cargo test --features local-development --lib -- tools::approval tools::wasm::tests::a_client
```

An installation can let one client skip the card for specific paths
(`approval.client_safe_paths` in `tools/outlayer.config.json`, for example
`{"esp32": ["/wallet/v1/transfer"]}`). It is off by default.

## The voice device

`local-clients/ironpaw-esp32` is Rust firmware for one board model, the
[Waveshare ESP32-S3-Touch-LCD-1.54](https://www.mercadolibre.com.ar/esp32-s3-touch-lcd-154-imu-procesamiento-de-audio--bateria/up/MLAU3902563256):
an ESP32-S3R8 with a 1.54" 240x240 touch LCD, microphone and speaker codecs, an IMU, a TF
card slot and a battery connector.
Hold a button and talk; Ironpaw transcribes the audio, the agent answers, and the reply
is shown and read aloud. Approvals and other questions from the agent appear as touch
buttons. The agent also keeps the board's notes and reminders, and the board rings the
reminders on its own, including Google Calendar meetings from a secret iCal URL. The
board has its own token that only works as client `esp32`.

## Built during the hackathon, and what existed before

Before November 1, 2026:

- Ironpaw itself: the daemon, typed bus, channels, automations, tools, encrypted secrets
  and the web and terminal clients (see [docs/architecture.md](docs/architecture.md)).
- The OutLayer tool and the host approval gate (commit `4f70afd`, October 22).

During the hackathon (November 1 to 14):

- The ESP32 firmware and its host simulator (`local-clients/ironpaw-esp32`).
- Server support for voice clients in `src/channels/local_client.rs`: `/client/transcribe`,
  `/client/speech` (Piper), the notes and events endpoints, the device token and the
  calendar feed.
- The `device_reminders` tool (`src/tools/built_in/device_reminders.rs`), so a reminder
  asked for in any conversation rings on the board.
- Speech to text through NEAR AI Cloud, with an optional local server (`whisper-server.py`).
- The per-client approval rule (`client_safe_paths`).

## Status and limits

- In this build, a wallet on **Mainnet** talks to a
  [local mock of the OutLayer API](https://github.com/matiasbenary/outlayer-mock) on
  `localhost:4400` (`MAINNET_BASE` in `wasm-tools/outlayer/src/lib.rs`). The recorded demo
  uses that mock. Transfers and Intents swaps were exercised against the mock, not against
  mainnet. **Testnet** wallets use the real OutLayer testnet API.
- OutLayer testnet has no Intents solvers, so the tool refuses swaps on testnet wallets.
- The approval is per request. There is no spending policy (limits, allowlists) and no
  durable audit log yet.
- One ESP32 board per Ironpaw instance. No wake word and no OTA updates.
- `run.sh` builds with `--features local-development`, which allows plain HTTP to loopback
  (the mock and the local Whisper server).

## How Ironpaw works

- **Channels** translate external messages into Ironpaw events and deliver replies.
- **Automations** decide what happens when a message arrives or a schedule fires.
- **The agent** is a bounded LLM loop: it reasons, calls allowed tools and returns.
- **Tools** (built-in, WASM, shell, runtime-created JavaScript) perform side effects.
  Network access, secrets and workspaces stay behind host-owned boundaries.

The parts communicate through a typed publish/subscribe bus. For the reasoning behind the
design, read [the architecture guide](docs/architecture.md).
