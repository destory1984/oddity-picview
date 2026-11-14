"""Time from launching a viewer on a folder until its thumbnails stop changing.

Works with any program (closed-source ones too): it never clicks or types. It
starts the program with the folder as argument, captures the window with
PrintWindow (works even when other windows cover it) every 50ms, and watches
one region of it. The result is the time of the last change in that region.

usage:
  python time_to_thumbnails.py <exe> <folder or -> <crop x0,y0,x1,y1 as 0..1> <out.png> [ENV=VALUE ...]

prints: seconds_to_last_change  cpu_seconds  peak_mem_MB  pid
"""
import ctypes
import ctypes.wintypes as wt
import hashlib
import os
import subprocess
import sys
import time

from PIL import Image

user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32
kernel32 = ctypes.windll.kernel32
psapi = ctypes.windll.psapi
user32.SetProcessDPIAware()

WIN_X, WIN_Y, WIN_W, WIN_H = 60, 60, 1600, 1000
STABLE = 2.0  # seconds without change = done
TIMEOUT = 60.0


def windows_of(pid):
    found = []

    @ctypes.WINFUNCTYPE(ctypes.c_bool, wt.HWND, wt.LPARAM)
    def cb(hwnd, _):
        p = wt.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(p))
        if p.value == pid and user32.IsWindowVisible(hwnd) and user32.GetWindowTextLengthW(hwnd) > 0:
            r = wt.RECT()
            user32.GetWindowRect(hwnd, ctypes.byref(r))
            if r.right - r.left > 300:
                found.append(hwnd)
        return True

    user32.EnumWindows(cb, 0)
    return found


class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [("biSize", wt.DWORD), ("biWidth", wt.LONG), ("biHeight", wt.LONG), ("biPlanes", wt.WORD),
                ("biBitCount", wt.WORD), ("biCompression", wt.DWORD), ("biSizeImage", wt.DWORD),
                ("biXPelsPerMeter", wt.LONG), ("biYPelsPerMeter", wt.LONG), ("biClrUsed", wt.DWORD),
                ("biClrImportant", wt.DWORD)]


def capture(hwnd):
    r = wt.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(r))
    w, h = r.right - r.left, r.bottom - r.top
    hdc = user32.GetWindowDC(hwnd)
    mem = gdi32.CreateCompatibleDC(hdc)
    bmp = gdi32.CreateCompatibleBitmap(hdc, w, h)
    old = gdi32.SelectObject(mem, bmp)
    user32.PrintWindow(hwnd, mem, 2)  # PW_RENDERFULLCONTENT
    bi = BITMAPINFOHEADER(ctypes.sizeof(BITMAPINFOHEADER), w, -h, 1, 32, 0, 0, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(w * h * 4)
    gdi32.GetDIBits(mem, bmp, 0, h, buf, ctypes.byref(bi), 0)
    gdi32.SelectObject(mem, old)
    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(mem)
    user32.ReleaseDC(hwnd, hdc)
    return Image.frombuffer("RGB", (w, h), buf, "raw", "BGRX", 0, 1)


class FILETIME(ctypes.Structure):
    _fields_ = [("lo", wt.DWORD), ("hi", wt.DWORD)]


class PMC(ctypes.Structure):
    _fields_ = [("cb", wt.DWORD), ("PageFaultCount", wt.DWORD), ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t), ("a", ctypes.c_size_t), ("b", ctypes.c_size_t),
                ("c", ctypes.c_size_t), ("d", ctypes.c_size_t), ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t)]


def proc_stats(pid):
    h = kernel32.OpenProcess(0x1000 | 0x0010, False, pid)  # QUERY_LIMITED_INFORMATION | VM_READ
    if not h:
        return float("nan"), float("nan")
    c, e, k, u = FILETIME(), FILETIME(), FILETIME(), FILETIME()
    kernel32.GetProcessTimes(h, ctypes.byref(c), ctypes.byref(e), ctypes.byref(k), ctypes.byref(u))
    cpu = ((k.hi << 32 | k.lo) + (u.hi << 32 | u.lo)) / 1e7
    pmc = PMC()
    pmc.cb = ctypes.sizeof(PMC)
    psapi.GetProcessMemoryInfo(h, ctypes.byref(pmc), pmc.cb)
    kernel32.CloseHandle(h)
    return cpu, pmc.PeakWorkingSetSize / 2**20


def main():
    exe, folder, crop, out = sys.argv[1:5]
    env = dict(os.environ)
    for kv in sys.argv[5:]:
        k, v = kv.split("=", 1)
        env[k] = v
    cx0, cy0, cx1, cy1 = map(float, crop.split(","))

    t0 = time.perf_counter()
    # "-" = start without a folder (the program reopens its last folder)
    proc = subprocess.Popen([exe] if folder == "-" else [exe, folder], env=env)
    hwnd = None
    while time.perf_counter() - t0 < 15:
        ws = windows_of(proc.pid)
        if ws:
            hwnd = ws[0]
            break
        if proc.poll() is not None:
            print("EXITED (handed over to a running instance?)")
            return
        time.sleep(0.01)
    if not hwnd:
        print("NO WINDOW")
        return
    user32.MoveWindow(hwnd, WIN_X, WIN_Y, WIN_W, WIN_H, True)

    last_hash, last_change = None, time.perf_counter()
    img = None
    while True:
        now = time.perf_counter()
        img = capture(hwnd)
        w, h = img.size
        region = img.crop((int(w * cx0), int(h * cy0), int(w * cx1), int(h * cy1)))
        digest = hashlib.md5(region.tobytes()).digest()
        if digest != last_hash:
            last_hash, last_change = digest, now
        if now - last_change >= STABLE or now - t0 > TIMEOUT:
            break
        time.sleep(0.05)
    cpu, peak = proc_stats(proc.pid)
    img.save(out)
    user32.PostMessageW(hwnd, 0x0010, 0, 0)  # WM_CLOSE: let it save settings and exit normally
    try:
        proc.wait(10)
    except subprocess.TimeoutExpired:
        pass
    print(f"{last_change - t0:.2f} {cpu:.2f} {peak:.0f} {proc.pid}")


main()
