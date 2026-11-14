## Companion Clients

This folder includes the source code for `ironpaw-tui`, `ironpaw-webui` and `ironpaw-esp32`.

The `ironpaw-tui` is a terminal-based user interface for interacting with the Ironpaw system.

`ironpaw-webui` is a standalone Vite application. It and `ironpaw-tui` use the
same Ironpaw `/client` API.

`ironpaw-esp32` is firmware for a Waveshare ESP32-S3-Touch-LCD-1.54: a push-to-talk voice
client over the same `/client` API. Ironpaw transcribes its audio. See its README.
