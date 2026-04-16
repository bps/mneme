#!/usr/bin/env python3
"""Resize probe for mneme integration tests.

Prints structured markers to stdout so tests can verify PTY resize behavior
without relying on shell signal semantics.

Protocol:
  READY              — printed on startup (probe is initialized)
  WINCH <rows> <cols> — printed on each SIGWINCH after querying TIOCGWINSZ
"""
import fcntl
import os
import signal
import struct
import termios
import time


def _query_winsize():
    """Query the terminal's current window size via TIOCGWINSZ."""
    buf = fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8)
    rows, cols = struct.unpack("HH", buf[:4])
    return rows, cols


def _winch_handler(_sig, _frame):
    try:
        rows, cols = _query_winsize()
        os.write(1, f"WINCH {rows} {cols}\n".encode())
    except Exception:
        pass


def main():
    signal.signal(signal.SIGWINCH, _winch_handler)
    os.write(1, b"READY\n")

    # Sleep forever, interruptible by signals.
    while True:
        try:
            time.sleep(3600)
        except InterruptedError:
            continue


if __name__ == "__main__":
    main()
