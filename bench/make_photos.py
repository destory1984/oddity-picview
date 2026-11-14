"""Make a folder of photo-sized JPEGs for benchmarks.

Each picture is 4032x3024 (a 12MP phone photo), quality 90, with smooth colour
areas plus fine noise so it compresses about like a real photo (2-4MB).

usage: python make_photos.py <out dir> [count=100]
"""
import os
import random
import sys

from PIL import Image, ImageFilter

out = sys.argv[1]
count = int(sys.argv[2]) if len(sys.argv) > 2 else 100
os.makedirs(out, exist_ok=True)
W, H = 4032, 3024

for k in range(count):
    rnd = random.Random(k)
    # coarse random colours, blown up smoothly = sky / walls / objects
    small = Image.new("RGB", (16, 12))
    small.putdata([tuple(rnd.randrange(256) for _ in range(3)) for _ in range(16 * 12)])
    img = small.resize((W, H), Image.BICUBIC)
    # sensor-like grain so the JPEG is not trivially small
    noise = Image.effect_noise((W, H), 28).convert("RGB")
    img = Image.blend(img, noise, 0.18).filter(ImageFilter.SMOOTH)
    img.save(os.path.join(out, f"photo_{k:03d}.jpg"), quality=90)
    print(k, end=" ", flush=True)
print()
