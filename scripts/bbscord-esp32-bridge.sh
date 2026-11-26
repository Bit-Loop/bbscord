#!/usr/bin/env bash
# Bridge a USB-Serial ESP32 16x2 HD44780 LCD terminal into `bbscord --geometry 16x2 --charset ascii`.
#
# Usage:
#   ./scripts/bbscord-esp32-bridge.sh /dev/ttyUSB0 [extra bbscord flags...]
#   ./scripts/bbscord-esp32-bridge.sh /dev/ttyACM0 --mock-demo

set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "Usage: $0 <serial-device> [extra bbscord args...]" >&2
  echo "Example: $0 /dev/ttyUSB0 --mock-demo" >&2
  exit 2
fi

DEVICE="$1"
shift
BBSCORD_BIN="${BBSCORD_BIN:-$(command -v bbscord || echo ./target/release/bbscord)}"

if [[ ! -x "$BBSCORD_BIN" ]]; then
  echo "error: bbscord binary not found at '$BBSCORD_BIN' (set BBSCORD_BIN or build release first)" >&2
  exit 1
fi

exec python3 - "$DEVICE" "$BBSCORD_BIN" "$@" <<'PY'
import fcntl, os, pty, select, signal, struct, subprocess, sys, termios

dev_path = sys.argv[1]
bin_path = sys.argv[2]
extra_args = sys.argv[3:]

ser_fd = os.open(dev_path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
attrs = termios.tcgetattr(ser_fd)
attrs[0] = 0
attrs[1] = 0
attrs[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
attrs[3] = 0
attrs[4] = termios.B115200
attrs[5] = termios.B115200
termios.tcsetattr(ser_fd, termios.TCSANOW, attrs)

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 2, 16, 0, 0))

env = os.environ.copy()
env["TERM"] = "vt100"
cmd = [bin_path, "--geometry", "16x2", "--charset", "ascii"] + extra_args
proc = subprocess.Popen(cmd, stdin=slave, stdout=slave, stderr=sys.stderr, env=env, close_fds=True)
os.close(slave)

def shutdown(_sig, _frame):
    if proc.poll() is None:
        proc.terminate()

signal.signal(signal.SIGINT, shutdown)
signal.signal(signal.SIGTERM, shutdown)

try:
    while proc.poll() is None:
        r, _, _ = select.select([master, ser_fd], [], [], 0.1)
        if master in r:
            try:
                data = os.read(master, 4096)
            except OSError:
                break
            if not data:
                break
            os.write(ser_fd, data)
        if ser_fd in r:
            try:
                keys = os.read(ser_fd, 256)
            except OSError:
                break
            if keys:
                os.write(master, keys)
finally:
    if proc.poll() is None:
        proc.terminate()
        proc.wait(timeout=2)
    os.close(master)
    os.close(ser_fd)
PY
