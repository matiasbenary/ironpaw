"""Arma preview/<estado>.gif (200x200, x5 vecino mas cercano, loop) desde los frames que devuelve capture()."""
import base64, io, json, sys
from pathlib import Path
from PIL import Image, ImageFilter

BG, INK = (16, 40, 72), (40, 22, 48)

def outline(im):
    """Contorno de 1 px alrededor del perro: pixel art sin borde se ve borroso a x5."""
    px, w, h = im.load(), *im.size
    mask = Image.new("L", im.size, 0)
    mp = mask.load()
    for y in range(h):
        for x in range(w):
            if max(abs(a - b) for a, b in zip(px[x, y], BG)) > 12:
                mp[x, y] = 255
    grown = mask.filter(ImageFilter.MaxFilter(3))
    out = im.copy()
    op = out.load()
    gp = grown.load()
    for y in range(h):
        for x in range(w):
            if gp[x, y] and not mp[x, y]:
                op[x, y] = INK
    return out

frames_json, out_dir = sys.argv[1], Path(sys.argv[2])
out_dir.mkdir(parents=True, exist_ok=True)
for name, frames in json.load(open(frames_json)).items():
    ims = []
    for f in frames:
        im = Image.open(io.BytesIO(base64.b64decode(f["png"].split(",", 1)[1]))).convert("RGB")
        if im.width >= 200:  # render ya a tamano final (perro realista): sin contorno ni escalado
            ims.append(im.quantize(128, dither=Image.FLOYDSTEINBERG))
            continue
        im = outline(im)
        im = im.quantize(16, dither=Image.NONE)  # ponytail: 16 colores fijos, bajar a 4 si se quiere look Game Boy
        ims.append(im.resize((im.width * 5, im.height * 5), Image.NEAREST))
    ims[0].save(out_dir / f"{name}.gif", save_all=True, append_images=ims[1:], duration=[f["ms"] for f in frames], loop=0)
    print(name, len(ims), "frames")
