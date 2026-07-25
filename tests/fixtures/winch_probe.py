"""Test child: a full-screen app that reports the window size it observes.

Used by the reattach test. It reads the size *inside* the SIGWINCH handler with
no fork, because a fork+exec of `stty` outlasts the window being measured — and
the whole point is whether the app can observe a change at all. An app that
sees no change repaints nothing, which is the black screen this guards against.
"""

import fcntl
import signal
import struct
import sys
import termios
import time

# Alternate screen, so spot treats this as a full-screen app.
sys.stdout.write("\033[?1049h")
sys.stdout.flush()


def on_winch(*_):
    rows, cols = struct.unpack("hh", fcntl.ioctl(0, termios.TIOCGWINSZ, b"0" * 4))
    sys.stdout.write("SIZE %d %d\r\n" % (rows, cols))
    sys.stdout.flush()


signal.signal(signal.SIGWINCH, on_winch)

while True:
    time.sleep(0.05)
