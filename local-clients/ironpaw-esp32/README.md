# ironpaw-esp32

Ironpaw voice client for one board model, the
[Waveshare ESP32-S3-Touch-LCD-1.54](https://www.mercadolibre.com.ar/esp32-s3-touch-lcd-154-imu-procesamiento-de-audio--bateria/up/MLAU3902563256)
(ESP32-S3R8, 1.54" 240x240 touch LCD, ES8311 and ES7210 audio codecs, IMU, TF card slot,
battery connector). It uses the same `/client`
API as `ironpaw-webui` and `ironpaw-tui`; the difference is that you talk to it.

There is no menu: everything starts with your voice.

The simulator (`sim/`, see below) runs the same screen code on the PC, so a layout change can be
checked before flashing.

1. Hold the +/KEY button and talk. A high beep means the mic is open, a low one that it
   closed. The VU meter shows at the top. Release.
2. The WAV (16 kHz mono, 10 s max) goes to `POST /client/transcribe`, which returns the text
   (NEAR AI's Whisper). The text goes straight to the agent (`POST .../messages`); the
   board no longer parses voice commands. Notes, reminders and timers are all the agent's job:
   - **Notes**: "Note: buy milk" is a message like any other; the agent appends it to
     `notes.md` in the board's workspace with its file tools. BOOT on the board pages through
     the latest 20 (`GET .../notes`).
   - **Reminders**: "Remind me of the dentist on Thursday at 10", "Move it to 11", "Cancel
     it", "What's coming up?" all go to the agent, which keeps `events.json` in the board's
     workspace (one `{"at": "YYYY-MM-DDTHH:MM", "text": "...", "repeat": "daily"|"weekly"}`
     per reminder; `repeat` is optional). The board reads the list (`GET .../events`, which
     also sets its clock) on connect, after every reply and every 10 min, keeps a copy on the
     SD card, and rings them itself: it barks (a synthesized dog), then the text is read out.
     A reminder with `repeat` rings every day or week at that time.
   - The screen shows what it understood (green), "Thinking..." with the seconds waited, and
     then the reply (white).

Controls:

- **+/KEY**: hold to talk. Pressing it while the board speaks cuts it off and records.
- **BOOT**: chat ↔ notes.
- **PWR**: hold 2.5 s to power off (GPIO5 reads it, GPIO2 drops the battery latch). On USB the
  board stays powered: the screen says so, unplug it to turn off. Press PWR to turn on again.
- **Tap**: top third goes back a page (or note), middle reads it aloud, bottom third goes forward.
- **Questions from the agent** (approvals, choices) show their options as buttons: tap one,
  or say the answer.
- The pet is a dog rendered in 3D (`assets/pet/3d/perro3d-realista.html`, Three.js), captured
  as 40 px pixel art (`assets/pet/3d/frames/`, see `assets/pet/README.md`) and converted with
  `assets/pet/m3.py` into the sprite sheet: idle, happy, eating, sleep, sad, listening, thinking
  and talking. The pet shows what the board is doing: it listens while you
  talk, looks aside while transcribing or waiting on the agent, talks while it speaks, is
  happy when a reminder rings, sad on an error and sleeps when the screen dims. It always sits
  under the header, with the mic level, what you said or the agent's question beside it.
- 20 s after the last reply or touch the dog takes the whole screen. Any tap brings the
  text back; tapping the small dog rests again. A new reply always shows the text.
- The dot in the top-right corner is Ironpaw: green online, yellow locked, red unreachable.
- Errors say what to do ("Ironpaw is locked: unlock it in the WebUI"); a tap in the middle
  of the screen shows the raw error. The header turns red on an error. When there is more than one page, chevrons at the right edge mark it.
- The backlight is fixed at `BRIGHTNESS` (8%); after 30 s idle the pet sleeps, any button or tap wakes it.
- Battery (ADC on GPIO1, 200K/100K divider, so VBAT is the pin's mV times 3): the header shows the
  percentage, as `+85%` while plugged in. Plugging in makes the pet eat and the status say
  "Charging 85%". Plugged means a USB host is enumerating (a PC) or VBAT is at least 4.3 V; a dumb
  charger on a full battery (4.2 V) may read as unplugged, and a charge-status pin would fix that.
- Recordings are trimmed of leading and trailing silence before upload (`SILENCE_DBFS`).
- If the WiFi drops, the board reconnects. A task watchdog (90 s) restarts it if it hangs.
- **TF card** (optional, FAT): `notes.json` and `events.json` (the last lists from Ironpaw,
  used when it is unreachable: "my notes" says "Offline copy"), `ironpaw.log` (every error
  shown on screen, with the time). Without a card the board works as before, just without those.
- The clock is the ESP32's own (there is no RTC chip on the board): Ironpaw sets it with the
  events list and the board keeps it across restarts while it has power. Unplug the battery
  and USB and it forgets the time until Ironpaw is reached again; reminders don't ring until then.

The reply is also read out loud. Ironpaw generates the voice on the PC with
[Piper](https://github.com/OHF-Voice/piper1-gpl) (`POST /client/speech`, 16 kHz PCM) and the board
plays it as it arrives. If Ironpaw has no Piper configured, the reply only shows on screen.
Pressing the button while it speaks cuts it off and starts recording. Ironpaw streams Piper's
audio as it is synthesized, so long replies start playing sooner.

## Ironpaw

In the Ironpaw runtime's `.env`:

```sh
IRONPAW_ADDR=0.0.0.0:7878                   # listen on the LAN, not just localhost
# The board's own token: it only works as client `esp32`, so a token pulled out of the
# board's flash can't read the WebUI's conversations or use the OAuth routes.
IRONPAW_DEVICE_TOKEN=<openssl rand -hex 32>
# Voice (optional): uv tool install piper-tts, plus a voice from huggingface.co/rhasspy/piper-voices
IRONPAW_PIPER=/home/<user>/.local/bin/piper
IRONPAW_PIPER_MODEL=/path/to/en_US-lessac-medium.onnx   # with its .onnx.json next to it
```

Plain HTTP on the LAN shows the token to anyone sniffing it. The board also speaks HTTPS
(public CAs, via ESP-IDF's certificate bundle): put Ironpaw behind a TLS proxy (Caddy, Tailscale
Serve) and provision an `https://` URL.

## Build, flash and set up

Rust toolchain for ESP-IDF (`espup`, `ldproxy`, `espflash`). On the first build, embuild
downloads ESP-IDF into `.embuild/`. To reuse an existing install, set
`ESP_IDF_TOOLS_INSTALL_DIR` in the environment (for example `custom:/path/to/.embuild/espressif`).

```sh
cargo build --release
espflash flash --port /dev/ttyACM0 --partition-table partitions.csv --erase-parts otadata \
  target/xtensa-esp32s3-espidf/release/ironpaw-esp32
# WiFi and Ironpaw settings go to NVS, not into the firmware.
PYTHON=<ESP-IDF python env>/bin/python \
WIFI_SSID=... WIFI_PASS=... IRONPAW_URL=http://<pc-ip>:7878 \
IRONPAW_TOKEN=<the IRONPAW_DEVICE_TOKEN> ./provision.sh /dev/ttyACM0
```

`WIFI_SSID`, `WIFI_PASS`, `IRONPAW_URL` and `IRONPAW_TOKEN` at build time still work as a
fallback when NVS is empty, but then they end up inside the binary: don't push `target/`.

## Files

| File | What it does |
|---|---|
| `src/main.rs` | `Board` (pins: speak, record, touch, battery) and `App` (one handler per input) |
| `src/net.rs` | NVS settings, WiFi, `/client` calls (notes, events, speech), clock sync, SSE thread with reconnect |
| `src/sd.rs` | TF card over 4-bit SDMMC (CLK 16, CMD 15, D0 17, D1 18, D2 13, D3 14), mounted at `/sdcard` |
| `src/clock.rs` | Local time without a zone database: parse and format, host-runnable tests |
| `src/audio.rs` | ES8311/ES7210 drivers (copied from pokesp), recording, silence trim, volume, playback, beep and bark |
| `src/ui.rs` | Framebuffer and the screen: header bar, pet, what you said, reply pages, option buttons |
| `sim/` | Host simulator of the board (see Simulator) |
| `src/app.rs` | The app: modes, inputs, the main loop. No ESP-IDF; talks to the hardware only through `Board` |
| `src/proto.rs` | Ironpaw's events and the SSE line parser, shared with the simulator |
| `src/wav.rs` | Sample rate, volume steps, WAV header and silence trim, shared with the simulator |
| `partitions.csv` | NVS plus two 3 MB app slots (no OTA anymore; the second slot is spare) |
| `provision.sh` | Writes WiFi and Ironpaw settings into NVS |
| `src/pet.rs` | The dog: which sprite frame shows for each mood, and the blit |
| `src/pet_data.rs`, `assets/pet/sheet.idx` | Generated by `assets/pet/m3.py` from `assets/pet/3d/frames/*.gif`, needs Pillow: don't hand-edit |
| `assets/pet/3d/` | The 3D dog (Three.js page), the capture-to-GIF script and the captured frames |
| `demo-call.sh` | Writes an `events.json` with a call in N minutes (heads-up after 1 min, the call itself after N) |
| `src/lcd.rs` | ST7789 over SPI+DMA (copied from pokesp) |

## Simulator

`sim/` runs the board's app logic (`src/app.rs`, with the screen, pet and protocol code) on the PC against a real Ironpaw, without a board:

```
cd sim
IRONPAW_CLIENT_TOKEN=... cargo +stable run     # IRONPAW_URL defaults to http://127.0.0.1:7878
feh -R 0.2 out/screen.ppm                       # in another terminal: the panel, redrawn on every change
```

The screen's text is also printed to the terminal whenever it changes. Type commands on stdin:

| Command | What it does |
|---|---|
| `talk <text>` | Holds +, says `<text>`, releases |
| `tap <x> <y>` | One tap at those pixels (240 by 240) |
| `boot` | Presses BOOT |
| `q` | Quits |

The sim also serves the screen at http://127.0.0.1:7979 (`IRONPAW_SIM_PORT` to change it): the panel at 2x, a click is a tap, a box and Talk button speak, and a BOOT button.

Faked: no speech recognition (`talk` sends the text as typed), no speech (replies are printed as `say: ...`),
no WiFi, and the battery is always 80% (`plug` and `unplug`, or the web page's Plug button, toggle the charger). The TF card is the `sim/sdcard/` folder.

## Known limits

- No wake word: talking always starts with +. ESP-SR's WakeNet runs on the S3 but needs its
  component, a model partition and the mic always on; not done yet.
- No OTA: flash over USB. The partition table still has two app slots in case it comes back.
- The conversation is always `esp32` with `client_id` `esp32`: one board per Ironpaw.
- Octal PSRAM assumed (ESP32-S3R8). If it doesn't boot, check `CONFIG_SPIRAM_MODE_OCT` in
  `sdkconfig.defaults`.
