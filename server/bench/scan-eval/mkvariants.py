from PIL import Image, ImageFilter
import os, random
here = os.path.dirname(os.path.abspath(__file__))
corpus = here + '/corpus'
random.seed(7)

def card(i):
    return Image.open(f'{corpus}/card{i}.png').convert('RGB')

def paste_photo(img, seed, scale=0.7, angle=8.0, blur=0.0, bg=None):
    rnd = random.Random(seed)
    W, H = 1100, 1500
    if bg is None:
        bg = Image.new('RGB', (W, H))
        px = bg.load()
        for y in range(H):
            for x in range(0, W, 4):
                v = 90 + rnd.randint(-25, 25) + (y * 20) // H
                for k in range(4):
                    if x + k < W: px[x + k, y] = (v, v - 8, v - 15)
    cw, ch = img.size
    tw = int(W * scale)
    th = int(tw * ch / cw)
    c = img.resize((tw, th), Image.LANCZOS).rotate(angle, expand=True, resample=Image.BICUBIC, fillcolor=(0, 0, 0))
    mask = c.convert('L').point(lambda p: 255 if p > 0 else 0)
    # perspective skew
    coeff = [1 + rnd.uniform(-.05, .05), rnd.uniform(-.06, .06), 0,
             rnd.uniform(-.03, .03), 1 + rnd.uniform(-.05, .05), rnd.uniform(-.02, .02),
             rnd.uniform(-8e-5, 8e-5), rnd.uniform(-6e-5, 6e-5)]
    c = c.transform(c.size, Image.QUAD, (0, 0, c.width, 0, c.width, c.height, 0, c.height), Image.BICUBIC)
    c = c.transform(c.size, Image.PERSPECTIVE, coeff, Image.BICUBIC)
    mask = mask.transform(c.size, Image.PERSPECTIVE, coeff, Image.BICUBIC)
    ox, oy = max(0, rnd.randint(-30, W - tw - 60)), max(0, rnd.randint(-20, H - th - 60))
    canvas = bg.copy()
    canvas.paste(c, (ox, oy), mask)
    if blur:
        canvas = canvas.filter(ImageFilter.GaussianBlur(blur))
    return canvas

v = []
v.append((card(0), 'full'))                      # plain card fills frame
v.append((paste_photo(card(1), 11), 'photo'))
v.append((paste_photo(card(2), 22, scale=0.55, angle=-14), 'photo2'))
v.append((paste_photo(card(3), 33, blur=1.6), 'blur'))
v.append((paste_photo(card(4), 44, scale=0.85, angle=3), 'photo3'))
v.append((card(5).transpose(Image.ROTATE_90), 'rot90'))
for i, (im, tag) in enumerate(v):
    im.save(f'{corpus}/scan{i}-{tag}.jpg', quality=88)
    print(tag, im.size)
