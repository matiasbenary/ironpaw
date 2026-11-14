#!/usr/bin/env python3
"""Speech to text on this machine: `POST /v1/audio/transcriptions` (OpenAI-compatible, the
`file` field) -> `{"text": ...}`. Point Ironpaw at it with `WHISPER_URL=http://127.0.0.1:8010`.
Uses faster-whisper on the GPU when it works, else the CPU. `WHISPER_MODEL` picks the model."""
import email.parser
import glob
import http.server
import json
import os
import site
import sys
import tempfile

PORT = int(os.environ.get("WHISPER_PORT", "8010"))
MODEL = os.environ.get("WHISPER_MODEL", "large-v3-turbo")

# ctranslate2 dlopens cuDNN/cuBLAS; the copies that come with torch are not on the loader's
# path, and LD_LIBRARY_PATH only counts at process start, so set it and start over once.
if "WHISPER_LIBS" not in os.environ:
    libs = [d for sp in site.getsitepackages() for d in glob.glob(f"{sp}/nvidia/*/lib")]
    os.environ["LD_LIBRARY_PATH"] = ":".join(libs + [os.environ.get("LD_LIBRARY_PATH", "")])
    os.environ["WHISPER_LIBS"] = "1"
    os.execv(sys.executable, [sys.executable] + sys.argv)

from faster_whisper import WhisperModel  # noqa: E402

try:
    # ponytail: int8_float32 because a GTX 1070 Ti has no fast float16; float16 on newer cards.
    model = WhisperModel(MODEL, device="cuda", compute_type="int8_float32")
    device = "cuda"
except Exception as error:  # no GPU, no CUDA libs: slow but works
    print(f"whisper: cuda unavailable ({str(error)[:80]}), using the cpu", flush=True)
    model = WhisperModel(MODEL, device="cpu", compute_type="int8")
    device = "cpu"


def transcribe(path):
    segments, info = model.transcribe(path, beam_size=1, vad_filter=True)
    text = " ".join(s.text.strip() for s in segments)
    print(f"whisper: {info.language} {info.duration:.1f}s -> {text!r}", flush=True)
    return text


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        raw = f"Content-Type: {self.headers['Content-Type']}\r\n\r\n".encode() + body
        audio = next((p.get_payload(decode=True) for p in email.parser.BytesParser().parsebytes(raw).get_payload()
                      if p.get_param("name", header="content-disposition") == "file"), None)
        if audio is None:
            return self.send_error(400, "no file field")
        with tempfile.NamedTemporaryFile(suffix=".wav") as f:
            f.write(audio)
            f.flush()
            text = transcribe(f.name)
        out = json.dumps({"text": text}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def log_message(self, *_):
        pass


print(f"whisper: {MODEL} on {device}, http://127.0.0.1:{PORT}", flush=True)
http.server.HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
