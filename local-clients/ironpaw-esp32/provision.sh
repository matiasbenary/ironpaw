#!/usr/bin/env bash
# Writes the WiFi and Ironpaw settings into the board's NVS partition, so they are not
# compiled into the firmware image (and an OTA image carries no secrets).
#
#   WIFI_SSID=... WIFI_PASS=... IRONPAW_URL=http://192.168.0.10:7878 \
#   IRONPAW_TOKEN=<IRONPAW_DEVICE_TOKEN> ./provision.sh [/dev/ttyACM0]
#
# Overwrites the whole NVS partition (the saved volume resets too).
set -euo pipefail
: "${WIFI_SSID:?}" "${WIFI_PASS:?}" "${IRONPAW_URL:?}" "${IRONPAW_TOKEN:?}"
port=${1:-/dev/ttyACM0}
# NVS offset and size, as in partitions.csv.
offset=0x9000
size=0x6000

# The generator ships with ESP-IDF's Python env (PYTHON=.embuild/.../bin/python), or
# `pip install esp-idf-nvs-partition-gen`.
python=${PYTHON:-python3}
if ! "$python" -c "import esp_idf_nvs_partition_gen" 2>/dev/null; then
  echo "missing the NVS generator: pip install esp-idf-nvs-partition-gen, or set PYTHON" >&2
  exit 1
fi

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
# The generator reads CSV: quote every value, doubling any quotes inside it.
quote() { printf '"%s"' "${1//\"/\"\"}"; }
{
  echo "key,type,encoding,value"
  echo "ironpaw,namespace,,"
  echo "wifi_ssid,data,string,$(quote "$WIFI_SSID")"
  echo "wifi_pass,data,string,$(quote "$WIFI_PASS")"
  echo "url,data,string,$(quote "$IRONPAW_URL")"
  echo "token,data,string,$(quote "$IRONPAW_TOKEN")"
} > "$dir/nvs.csv"
"$python" -m esp_idf_nvs_partition_gen generate "$dir/nvs.csv" "$dir/nvs.bin" "$size" >/dev/null
espflash write-bin --port "$port" "$offset" "$dir/nvs.bin"
echo "Provisioned. Restart the board."
