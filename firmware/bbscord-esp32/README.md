# `bbscord-esp32` — `16×2` HD44780 Hardware LCD & 6-Button Bridge Firmware

Turns any **ESP32**, **ESP32-S3**, **ESP32-C3**, or **ESP32-C6** + a standard I²C `16×2` HD44780 character LCD (PCF8574 backpack @ `0x27` or `0x3F`) + 6 tactile buttons into a physical pocket/desk BBS Discord terminal powered by `bbscord --geometry 16x2 --charset ascii`.

> **Why a hardware terminal bridge?** Full `bbscord` runs BoringSSL/TLS 1.3 fingerprinting, Opus + DAVE MLS E2EE voice, GStreamer WebRTC, and a 128 MiB media cache—workloads that require a host OS (Linux/macOS/Windows/Raspberry Pi). This firmware turns an ESP32 into a zero-flicker `16×2` VT100/ANSI hardware display + 6-key controller that drives `bbscord`'s built-in 7-bit HD44780 Micro Mode over USB-Serial (`115200` baud).

## Default Wiring

Connect each button between its GPIO pin and `GND` (internal pull-ups enabled).

| Chip | I²C SDA | I²C SCL | `↑` UP | `↓` DOWN | `←` LEFT | `→` RIGHT | `Enter` | `Esc` |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| **ESP32** (`esp32`) | GPIO 21 | GPIO 22 | GPIO 13 | GPIO 14 | GPIO 25 | GPIO 26 | GPIO 27 | GPIO 33 |
| **ESP32-S3** (`esp32s3`) | GPIO 8 | GPIO 9 | GPIO 4 | GPIO 5 | GPIO 6 | GPIO 7 | GPIO 15 | GPIO 16 |
| **ESP32-C3** (`esp32c3`) | GPIO 8 | GPIO 9 | GPIO 2 | GPIO 3 | GPIO 4 | GPIO 5 | GPIO 6 | GPIO 7 |
| **ESP32-C6** (`esp32c6`) | GPIO 6 | GPIO 7 | GPIO 0 | GPIO 1 | GPIO 2 | GPIO 3 | GPIO 4 | GPIO 5 |

## Flashing a Release Binary

Download `bbscord-esp32-<version>-<chip>-factory.bin` from GitHub Releases and flash at offset `0x0`:

```bash
# Example for classic ESP32 (replace --chip with esp32s3 / esp32c3 / esp32c6 as needed)
esptool.py --chip esp32 --port /dev/ttyUSB0 --baud 460800 write_flash 0x0 bbscord-esp32-1.0.0-esp32-factory.bin
```

## Connecting to `bbscord`

```bash
# Try offline demo on your physical 16x2 LCD right away:
./scripts/bbscord-esp32-bridge.sh /dev/ttyUSB0 --mock-demo

# Or connect to live Discord:
./scripts/bbscord-esp32-bridge.sh /dev/ttyUSB0
```

## Building from Source

```bash
cd firmware/bbscord-esp32
pio run -e esp32    # or esp32s3, esp32c3, esp32c6
```
