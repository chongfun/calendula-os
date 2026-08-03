import fcntl
import os
import struct
import termios
import time

PORT = "/dev/cu.usbmodem101"
LOG = "/tmp/x4raw.log"

# Held open for the life of the process. This capture runs until it is killed
# and appends from two places -- the read loop and the reconnect path below --
# so there is no scope a context manager could close it at. Unbuffered so a
# `tail -f` sees lines as they land and a kill loses nothing.
out = open(LOG, "ab", buffering=0)  # noqa: SIM115


def attach_once() -> None:
    fd = os.open(PORT, os.O_RDONLY | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        attrs = termios.tcgetattr(fd)
        attrs[0] = 0  # iflag: raw
        attrs[1] = 0  # oflag: raw
        attrs[2] = termios.CREAD | termios.CLOCAL | termios.CS8  # cflag
        attrs[3] = 0  # lflag: raw
        termios.tcsetattr(fd, termios.TCSANOW, attrs)
        # USB-JTAG-Serial gates console TX on DTR (terminal present), while
        # RTS participates in the reset/download sequences. Keep DTR set and
        # RTS clear so we receive output without poking the chip.
        fcntl.ioctl(fd, termios.TIOCMBIS, struct.pack("i", termios.TIOCM_DTR))
        fcntl.ioctl(fd, termios.TIOCMBIC, struct.pack("i", termios.TIOCM_RTS))
        out.write(b"\n[x4raw attached]\n")
        idle = 0.0
        while True:
            try:
                data = os.read(fd, 4096)
            except BlockingIOError:
                time.sleep(0.05)
                continue
            if data:
                out.write(data)
                idle = 0.0
            else:
                time.sleep(0.05)
                idle += 0.05
    finally:
        os.close(fd)


while True:
    try:
        attach_once()
    except OSError as err:
        out.write(f"\n[x4raw port unavailable: {err}]\n".encode())
        time.sleep(0.5)
