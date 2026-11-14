# The pet

The dog on the screen is rendered in 3D by `3d/perro3d-realista.html` (Three.js, open it in a
browser) and captured as 40x40 pixel art, 16 frames per mood.

1. Open the page and, in the console, run `capture("<mood>")` for each of `idle`, `happy`,
   `hungry`, `sleep`, `sad`, `listening`, `thinking`, `talking`. Save the results as one JSON
   object `{"<mood>": [{"ms": ..., "png": "data:image/png;base64,..."}, ...], ...}`.
2. `python3 3d/sprites.py frames.json 3d/frames` writes a 200x200 GIF per mood (x5 so a human
   can check it).
3. `python3 m3.py` turns `3d/frames/*.gif` into `sheet.idx` and `../../src/pet_data.rs`
   (one 63-colour palette for every mood). Needs Pillow.

`hungry` is the board's `Eating` mood (the battery is charging).
