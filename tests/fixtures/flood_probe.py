"""Test child: a full-screen app that floods output and counts its size changes.

Used by the lag-drop test. The alternate screen is what makes spot classify it
as a painter, which is the case that matters: when the daemon throws away a
backlog it cannot replay a painter's ring (those are stale frames), so the
client is left holding half a frame that nothing will redraw unless spot forces
the issue.

Two details the test depends on. The flood waits on a line of input, so it
happens while a client is attached and a backlog can actually build. And the
winch count is reported *cumulatively, on repeat*, rather than one line per
signal: any single report is liable to be inside the backlog that gets dropped,
which is the whole point of the exercise.
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

winches = 0
last_size = (0, 0)


def on_winch(*_):
    # Counts only. Writing from the handler is what winch_probe.py does, but it
    # is idle when the signal lands; here the signal arrives *during* a write,
    # and re-entering the buffered writer raises RuntimeError.
    global winches, last_size
    winches += 1
    last_size = struct.unpack("hh", fcntl.ioctl(0, termios.TIOCGWINSZ, b"0" * 4))


signal.signal(signal.SIGWINCH, on_winch)

sys.stdin.readline()

# The count as it stands before any lag-drop. The pause is not optional: start
# flooding immediately and the backlog overflows while this line is still in it,
# so the drop under test takes the baseline with it.
sys.stdout.write("BASE %d\r\n" % winches)
sys.stdout.flush()
time.sleep(0.5)

# ~8 MiB: comfortably past MAX_QUEUE even after the client's terminal, both
# socket buffers and the daemon's backlog have each taken what they can hold.
chunk = "x" * 4096 + "\r\n"
for _ in range(2048):
    sys.stdout.write(chunk)
sys.stdout.flush()

while True:
    sys.stdout.write("WINCH %d SIZE %d %d\r\n" % (winches, last_size[0], last_size[1]))
    sys.stdout.flush()
    time.sleep(0.25)
