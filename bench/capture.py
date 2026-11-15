"""Open the viewer, wait, and save two screenshots of its window 0.3s apart.

Sends no mouse or keyboard input (PrintWindow capture works even when other
windows cover it), so it is safe to run while someone uses the PC. Settings go
to a separate folder next to the output, never the user's own settings.

usage:
  python bench/capture.py <exe> <folder or file> <out.png>
  -> writes out_a.png and out_b.png, prints whether the two frames differ

Screenshot switches the app understands (set them in the environment):
  PICVIEW_SHOW_MENU=1        open the "더보기" popup at start
  PICVIEW_SHOW_MENU=recent   open the "최근" popup at start
  PICVIEW_SHOW_SETTINGS=1    open the settings window at start
  PICVIEW_SHOW_ABOUT=1       with PICVIEW_SHOW_SETTINGS=1: also the About window
"""
import ctypes
import hashlib
import os
import subprocess
import sys
import time

here = os.path.dirname(os.path.abspath(__file__))
src = open(os.path.join(here, "time_to_thumbnails.py"), encoding="utf-8").read().replace("\nmain()\n", "\n")
ttt = {}
exec(compile(src, "time_to_thumbnails", "exec"), ttt)  # reuse windows_of() and capture()

exe, target, out = sys.argv[1:4]
env = dict(os.environ, PICVIEW_DATA_DIR=os.path.join(os.path.dirname(os.path.abspath(out)), "pv_data_capture"))
p = subprocess.Popen([exe, target], env=env)
hwnd = None
for _ in range(500):
    ws = ttt["windows_of"](p.pid)
    if ws:
        hwnd = ws[0]
        break
    time.sleep(0.01)
if not hwnd:
    sys.exit("no window")
ctypes.windll.user32.MoveWindow(hwnd, 60, 60, 1200, 800, True)
time.sleep(2.0)
a = ttt["capture"](hwnd)
time.sleep(0.3)
b = ttt["capture"](hwnd)
stem = out[:-4] if out.lower().endswith(".png") else out
a.save(stem + "_a.png")
b.save(stem + "_b.png")
print("frames differ:", hashlib.md5(a.tobytes()).digest() != hashlib.md5(b.tobytes()).digest())
ctypes.windll.user32.PostMessageW(hwnd, 0x0010, 0, 0)  # WM_CLOSE
p.wait(10)
