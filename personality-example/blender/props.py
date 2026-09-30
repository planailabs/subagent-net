"""The room (floor, walls, window opening, rug, sky) and the things in it.

Every thing is a top-level node named by its world object id, origin at
the centre of its footprint on the surface it stands on, front towards
-Y. Parts the browser changes are child nodes: `pot`, `coffee`, `led`
(coffee maker), `mug_fill`, `platter`, `shade`, `flame`, `glass`, `sky`.
The browser places things from the world state; the layout here mirrors
`vesper_room::world` only for the preview renders.
"""

import math
import random

import bpy
from mathutils import Vector

from build import Builder, ctx, image, material, preview

# id -> (x, y, z) in world coordinates (y up, z towards the viewers), rotation.
LAYOUT = {
    "counter": ((-2.9, 0.0, -2.6), 0.0),
    "coffee_maker": ((-3.4, 0.9, -2.65), 0.0),
    "mug": ((-2.7, 0.9, -2.6), 0.0),
    "window": ((-0.6, 1.0, -3.0), 0.0),
    "record_player": ((0.9, 0.0, -2.7), 0.0),
    "bookshelf": ((2.3, 0.0, -2.8), 0.0),
    "lamp": ((3.6, 0.0, -2.5), 0.0),
    "armchair": ((3.0, 0.0, -1.2), -math.pi / 2),
    "side_table": ((3.1, 0.0, 0.0), 0.0),
}


def W(x, y, z):
    """World coordinates to Blender's."""
    return Vector((x, -z, y))


def materials():
    rng = random.Random(3)
    shades = [rng.uniform(0.75, 1.0) for _ in range(16)]

    def planks(x, y):
        row = y // 16
        joint = (x + row * 37) % 128 < 2
        edge = y % 16 == 0
        k = 0.35 if joint or edge else shades[(row * 3 + (x + row * 37) // 128) % 16]
        grain = 0.94 + 0.06 * math.sin(x * 0.35 + row * 1.7)
        return (0.20 * k * grain, 0.12 * k * grain, 0.075 * k * grain)

    return {
        "floor": material("r_floor", (1, 1, 1), rough=0.6, image=image("r_planks", (128, 128), planks)),
        "wall": material("r_wall", (0.17, 0.13, 0.19), rough=0.9),
        "trim": material("r_trim", (0.03, 0.03, 0.035), rough=0.5),
        "rug": material("r_rug", (0.28, 0.035, 0.06), rough=0.95),
        "rug_border": material("r_rug_border", (0.08, 0.02, 0.1), rough=0.95),
        "sky": material("r_sky", (0.05, 0.07, 0.16), rough=1.0),
        "wood": material("p_wood", (0.11, 0.06, 0.035), rough=0.55),
        "black": material("p_black", (0.02, 0.02, 0.025), rough=0.35),
        "stone": material("p_stone", (0.24, 0.24, 0.26), rough=0.3),
        "metal": material("p_metal", (0.75, 0.75, 0.78), rough=0.25, metal=1.0),
        "glass": material("p_glass", (0.6, 0.7, 0.8), rough=0.05, alpha=0.25, double=True),
        "coffee": material("p_coffee", (0.09, 0.045, 0.02), rough=0.15),
        "led": material("p_led", (0.9, 0.05, 0.05), emit=(1.0, 0.1, 0.05)),
        "mug": material("p_mug", (0.04, 0.035, 0.05), rough=0.3),
        "velvet": material("p_velvet", (0.32, 0.025, 0.07), rough=0.95),
        "shade": material("p_shade", (0.88, 0.8, 0.62), rough=0.8, double=True),
        "candle": material("p_candle", (0.9, 0.86, 0.75), rough=0.7),
        "flame": material("p_flame", (1.0, 0.6, 0.2), emit=(1.0, 0.55, 0.15)),
        "label": material("p_label", (0.6, 0.05, 0.08), rough=0.5),
        "bone": material("p_bone", (0.85, 0.82, 0.72), rough=0.6),
        "frame_art": material("p_art", (0.2, 0.05, 0.25), rough=0.8),
        **{f"book{i}": material(f"p_book{i}", c, rough=0.7) for i, c in enumerate([(0.3, 0.03, 0.05), (0.05, 0.12, 0.08), (0.05, 0.06, 0.2), (0.18, 0.06, 0.25), (0.03, 0.03, 0.03), (0.4, 0.28, 0.08), (0.35, 0.33, 0.3)])},
    }


def mesh(name, coll, mats, fn, parent=None, at=(0, 0, 0)):
    b = Builder()
    fn(b)
    o = b.object(name, coll, mats)
    if parent:
        o.parent = parent
    o.location = at
    return o


def things(coll, mats):
    made = {}

    def counter(b):
        b.box((0, 0, 0.44), (2.0, 0.7, 0.84), "black")
        b.box((0, 0, 0.88), (2.04, 0.72, 0.04), "stone")
        for x in (-0.75, -0.25, 0.25, 0.75):
            b.box((x, -0.352, 0.44), (0.47, 0.01, 0.78), "wood")
            b.box((x + 0.18, -0.36, 0.7), (0.02, 0.02, 0.1), "metal")

    made["counter"] = mesh("counter", coll, mats, counter)

    def coffee_maker(b):
        b.box((0, 0, 0.02), (0.22, 0.24, 0.04), "black")
        b.box((0, 0.08, 0.2), (0.22, 0.08, 0.36), "black")
        b.box((0, 0, 0.36), (0.22, 0.24, 0.07), "black")
        b.cylinder((0, -0.03, 0.041), 0.07, 0.07, 0.004, "metal")

    cm = made["coffee_maker"] = mesh("coffee_maker", coll, mats, coffee_maker)

    def pot(b):
        b.cylinder((0, 0, 0.075), 0.068, 0.058, 0.15, "glass", segs=20)
        b.box((0.085, 0, 0.09), (0.02, 0.025, 0.09), "black")
        b.cylinder((0, 0, 0.155), 0.06, 0.06, 0.012, "black", segs=20)

    p = mesh("pot", coll, mats, pot, cm, (0, -0.03, 0.045))
    mesh("coffee", coll, mats, lambda b: b.cylinder((0, 0, 0.045), 0.062, 0.056, 0.09, "coffee", segs=20), p, (0, 0, 0.004))
    mesh("led", coll, mats, lambda b: b.ellipsoid((0, 0, 0), (0.008, 0.004, 0.008), "led", segs=8, rings=6), cm, (0.07, 0.038, 0.3))

    def mug(b):
        b.cylinder((0, 0, 0.05), 0.04, 0.038, 0.1, "mug", segs=18)
        b.loft([((0.052, 0, 0.02), 0.012, 0.006), ((0.068, 0, 0.05), 0.012, 0.006), ((0.052, 0, 0.08), 0.012, 0.006)], "mug", segs=8)

    m = made["mug"] = mesh("mug", coll, mats, mug)
    mesh("mug_fill", coll, mats, lambda b: b.cylinder((0, 0, 0), 0.036, 0.036, 0.004, "coffee", segs=18), m, (0, 0, 0.09))

    def window(b):
        for x in (-0.67, 0.67, 0.0):
            b.box((x, 0, 0.6), (0.06 if x else 0.04, 0.12, 1.2), "black")
        for z in (0.03, 1.17, 0.62):
            b.box((0, 0, z), (1.4, 0.12, 0.06 if z != 0.62 else 0.04), "black")
        b.box((0, -0.1, -0.02), (1.5, 0.22, 0.04), "black")

    w = made["window"] = mesh("window", coll, mats, window)
    mesh("glass", coll, mats, lambda b: b.box((0, 0, 0.6), (1.3, 0.01, 1.1), "glass"), w)

    def record_player(b):
        for x in (-0.4, 0.4):
            for y in (-0.2, 0.2):
                b.cylinder((x, y, 0.03), 0.02, 0.015, 0.06, "black", segs=8)
        b.box((0, 0, 0.29), (0.9, 0.5, 0.46), "wood")
        b.box((0, -0.251, 0.29), (0.86, 0.01, 0.42), "black")
        b.box((0, 0.02, 0.545), (0.46, 0.36, 0.05), "black")
        b.box((0.16, 0.08, 0.58), (0.02, 0.22, 0.012), "metal", rot=(0, 0, 0.35))
        b.cylinder((0.18, 0.15, 0.585), 0.02, 0.02, 0.03, "metal", segs=10)
        # Sleeves leaning in the open cabinet.
        for i, c in enumerate(("label", "book2", "book3", "book4")):
            b.box((-0.3 + i * 0.03, -0.1, 0.25), (0.012, 0.3, 0.3), c, rot=(0, 0.12, 0))

    rp = made["record_player"] = mesh("record_player", coll, mats, record_player)

    def platter(b):
        b.cylinder((0, 0, 0), 0.15, 0.15, 0.012, "black", segs=32)
        b.cylinder((0, 0, 0.007), 0.045, 0.045, 0.004, "label", segs=16)
        b.cylinder((0.03, 0, 0.01), 0.008, 0.008, 0.004, "bone", segs=8)

    mesh("platter", coll, mats, platter, rp, (-0.05, 0.02, 0.578))

    def bookshelf(b):
        rng = random.Random(11)
        b.box((-0.59, 0, 0.95), (0.03, 0.36, 1.9), "wood")
        b.box((0.59, 0, 0.95), (0.03, 0.36, 1.9), "wood")
        b.box((0, 0.17, 0.95), (1.2, 0.02, 1.9), "wood")
        for i in range(6):
            z = 0.04 + i * 0.37
            b.box((0, 0, z), (1.18, 0.34, 0.03), "wood")
            if i == 5:
                continue
            x = -0.56
            while x < 0.5:
                w = rng.uniform(0.025, 0.05)
                h = rng.uniform(0.2, 0.3)
                lean = 0.0 if rng.random() > 0.1 else 0.2
                b.box((x + w / 2, -0.01, z + 0.015 + h / 2), (w, rng.uniform(0.2, 0.26), h), f"book{rng.randrange(7)}", rot=(0, lean, 0))
                x += w + 0.004 + (0.08 if rng.random() > 0.9 else 0)
        # A small skull on top.
        b.ellipsoid((0.3, 0, 1.98), (0.05, 0.06, 0.055), "bone", segs=12, rings=8)
        b.ellipsoid((0.3, -0.035, 1.945), (0.035, 0.035, 0.025), "bone", segs=10, rings=6)
        for sx in (-1, 1):
            b.ellipsoid((0.3 + 0.019 * sx, -0.052, 1.98), (0.012, 0.01, 0.012), "black", segs=8, rings=6)

    made["bookshelf"] = mesh("bookshelf", coll, mats, bookshelf)

    def lamp(b):
        b.cylinder((0, 0, 0.015), 0.15, 0.15, 0.03, "black", segs=20)
        b.cylinder((0, 0, 0.78), 0.012, 0.012, 1.5, "black", segs=8)
        b.ellipsoid((0, 0, 1.5), (0.03, 0.03, 0.05), "flame", segs=10, rings=6)

    lp = made["lamp"] = mesh("lamp", coll, mats, lamp)
    mesh("shade", coll, mats, lambda b: b.cylinder((0, 0, 0), 0.22, 0.13, 0.3, "shade", segs=24), lp, (0, 0, 1.52))

    def armchair(b):
        b.box((0, 0, 0.25), (0.86, 0.8, 0.3), "velvet")
        b.box((0, -0.02, 0.44), (0.64, 0.7, 0.1), "velvet")
        b.box((0, 0.33, 0.78), (0.86, 0.16, 0.76), "velvet", rot=(-0.1, 0, 0))
        b.ellipsoid((0, 0.36, 1.14), (0.43, 0.09, 0.08), "velvet", segs=16, rings=8)
        for sx in (-1, 1):
            b.box((0.36 * sx, -0.02, 0.5), (0.14, 0.76, 0.22), "velvet")
            b.ellipsoid((0.36 * sx, -0.02, 0.61), (0.075, 0.38, 0.05), "velvet", segs=12, rings=6)
            for y in (-0.32, 0.32):
                b.cylinder((0.36 * sx, y, 0.05), 0.025, 0.018, 0.1, "black", segs=8)

    made["armchair"] = mesh("armchair", coll, mats, armchair)

    def side_table(b):
        b.cylinder((0, 0, 0.585), 0.3, 0.3, 0.03, "black", segs=28)
        b.cylinder((0, 0, 0.3), 0.03, 0.03, 0.56, "black", segs=10)
        b.cylinder((0, 0, 0.015), 0.18, 0.18, 0.03, "black", segs=20)
        b.cylinder((0.1, 0.05, 0.66), 0.025, 0.025, 0.12, "candle", segs=12)
        b.cylinder((0.1, 0.05, 0.605), 0.05, 0.05, 0.01, "metal", segs=16)

    st = made["side_table"] = mesh("side_table", coll, mats, side_table)
    mesh("flame", coll, mats, lambda b: b.ellipsoid((0, 0, 0), (0.008, 0.008, 0.018), "flame", segs=8, rings=6), st, (0.1, 0.05, 0.738))

    for id, (pos, rot) in LAYOUT.items():
        made[id].location = W(*pos)
        made[id].rotation_euler = (0, 0, rot)
    return made


def shell(coll, mats):
    def room(b):
        floor = b.box((0, 0, -0.01), (8, 6, 0.02), "floor")
        uv = b.uv
        for f in {f for v in floor for f in v.link_faces}:
            for loop in f.loops:
                loop[uv].uv = (loop.vert.co.x / 2, loop.vert.co.y / 2)
        # Back wall (Blender +Y) with the window opening, side walls.
        h = 2.8
        back = 3.05
        for x0, x1, z0, z1 in ((-4.1, -1.3, 0, h), (0.1, 4.1, 0, h), (-1.3, 0.1, 0, 1.0), (-1.3, 0.1, 2.2, h)):
            b.box(((x0 + x1) / 2, back, (z0 + z1) / 2), (x1 - x0, 0.1, z1 - z0), "wall")
        for sx in (-1, 1):
            b.box((4.05 * sx, 0, h / 2), (0.1, 6.2, h), "wall")
            b.box((3.99 * sx, 0, 0.05), (0.02, 6.0, 0.1), "trim")
        b.box((0, 2.99, 0.05), (8.0, 0.02, 0.1), "trim")
        # Rug in the middle, a picture on the right wall.
        b.box((0.4, 0.3, 0.004), (2.6, 1.8, 0.008), "rug_border")
        b.box((0.4, 0.3, 0.006), (2.4, 1.6, 0.008), "rug")
        b.box((3.98, 0.6, 1.65), (0.04, 0.9, 0.66), "black")
        b.box((3.955, 0.6, 1.65), (0.02, 0.8, 0.56), "frame_art")

    r = mesh("room", coll, mats, room)
    mesh("sky", coll, mats, lambda b: b.box((0, 0, 0), (3.0, 0.02, 2.0), "sky"), r, (-0.6, 3.7, 1.6))
    return r


def build(scene):
    mats = materials()
    room = shell(scene.collection, mats)
    made = things(scene.collection, mats)
    return room, made


def preview_all(scene, out):
    vesper = [bpy.data.objects["vesper_rig"], bpy.data.objects["vesper"]]
    for o in vesper:
        scene.collection.objects.link(o)
    vesper[0].location = W(-2.6, 0, -1.6)
    vesper[0].rotation_euler = (0, 0, math.pi)
    preview(scene, out / "room.png", W(0.5, 2.6, 5.5), W(0, 0.8, -1.2), 26)
    preview(scene, out / "kitchen.png", W(-1.2, 1.7, 0.2), W(-3.0, 0.9, -2.4), 30)
    preview(scene, out / "reading.png", W(1.2, 1.6, 1.2), W(3.0, 0.7, -1.4), 30)
    vesper[0].location = (0, 0, 0)
    vesper[0].rotation_euler = (0, 0, 0)
    for o in vesper:
        scene.collection.objects.unlink(o)
