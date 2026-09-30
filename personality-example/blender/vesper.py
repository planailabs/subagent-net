"""Vesper: a woman of about 25, sculpted rather than assembled: one
continuous body grown from a skin-modifier skeleton, subdivided and shaped
with sculpt-style brushes, heat-weighted to the rig. A dense head sculpted
the same way (jaw, cheekbones, eye sockets, nose, lips, a real mouth slit),
a blue bob with blunt bangs and strand grooves, an off-shoulder black dress
with long sleeves and a folded skirt with a violet hem, fishnet tights,
platform boots, a choker. One skinned mesh, clips, and a `mouth_open` shape
key that drops the jaw.

Brushes are plain functions over vertex positions, so the sculpt is
reproducible from this file.
"""

import math

import bmesh
import bpy
from mathutils import Euler, Matrix, Quaternion, Vector
from mathutils.kdtree import KDTree

from build import Builder, ctx, image, material, preview

FPS = 30

# Bones: name -> (head, tail, parent). Left is +X (she faces -Y).
BONES = {
    "hips": ((0, 0, 0.93), (0, 0, 1.02), None),
    "spine": ((0, 0, 1.02), (0, 0, 1.22), "hips"),
    "chest": ((0, 0, 1.22), (0, 0, 1.40), "spine"),
    "neck": ((0, 0, 1.40), (0, 0, 1.49), "chest"),
    "head": ((0, 0, 1.49), (0, 0, 1.76), "neck"),
}
for s, sx in (("L", 1), ("R", -1)):
    BONES |= {
        f"upper_arm_{s}": ((0.18 * sx, 0, 1.37), (0.215 * sx, 0, 1.11), "chest"),
        f"forearm_{s}": ((0.215 * sx, 0, 1.11), (0.24 * sx, -0.01, 0.87), f"upper_arm_{s}"),
        f"hand_{s}": ((0.24 * sx, -0.01, 0.87), (0.245 * sx, -0.01, 0.76), f"forearm_{s}"),
        f"thigh_{s}": ((0.085 * sx, 0, 0.91), (0.08 * sx, 0, 0.49), "hips"),
        f"shin_{s}": ((0.08 * sx, 0, 0.49), (0.075 * sx, 0.01, 0.09), f"thigh_{s}"),
        f"foot_{s}": ((0.075 * sx, 0.01, 0.09), (0.075 * sx, -0.11, 0.03), f"shin_{s}"),
    }

HEAD_C = Vector((0, 0.005, 1.60))
HEAD_R = Vector((0.078, 0.097, 0.106))
MOUTH_Z = HEAD_C.z - 0.054


def materials():
    def fishnet(x, y):
        on = (x + y) % 8 == 0 or (x - y) % 8 == 0
        return (0.02, 0.02, 0.025) if on else (0.9, 0.78, 0.74)

    def iris(x, y):
        # Rows from the front pole of the eyeball backwards.
        r = y / 63
        if r > 0.95:
            return (0.01, 0.01, 0.015)  # pupil
        if r > 0.83:
            k = (r - 0.83) / 0.12
            return (0.30 - 0.15 * k, 0.16 - 0.08 * k, 0.55 - 0.2 * k)  # violet iris, darker inward
        if r > 0.81:
            return (0.08, 0.05, 0.12)  # limbal ring
        return (0.93, 0.92, 0.94)

    return {
        "skin": material("v_skin", (0.93, 0.81, 0.77), rough=0.5),
        "dress": material("v_dress", (0.025, 0.022, 0.03), rough=0.4),
        "hem": material("v_hem", (0.24, 0.05, 0.34), rough=0.5),
        "tights": material("v_tights", (1, 1, 1), rough=0.7, image=image("v_fishnet", (32, 32), fishnet)),
        "boots": material("v_boots", (0.015, 0.015, 0.02), rough=0.2, metal=0.1),
        "sole": material("v_sole", (0.05, 0.05, 0.06), rough=0.8),
        "hair": material("v_hair", (0.08, 0.26, 0.8), rough=0.38, double=True),
        "eye": material("v_eye", (1, 1, 1), rough=0.1, image=image("v_iris", (8, 64), iris)),
        "liner": material("v_liner", (0.01, 0.01, 0.015), rough=0.4),
        "lips": material("v_lips", (0.26, 0.035, 0.13), rough=0.3),
        "mouth": material("v_mouth", (0.08, 0.01, 0.02), rough=0.6),
        "silver": material("v_silver", (0.8, 0.8, 0.85), rough=0.2, metal=1.0),
    }


# --- sculpting ------------------------------------------------------------------


def smoothstep(x):
    x = max(0.0, min(1.0, x))
    return x * x * (3 - 2 * x)


def brush(verts, centre, radius, move, scale=(1, 1, 1)):
    """A grab brush: moves vertices near `centre` by `move`, fading smoothly
    to nothing at `radius`. `scale` stretches the footprint per axis."""
    c, m = Vector(centre), Vector(move)
    for v in verts:
        d = v.co - c
        d = Vector((d.x / scale[0], d.y / scale[1], d.z / scale[2])).length
        if d < radius:
            v.co += m * (1 - smoothstep(d / radius))


def relax(bm, verts, times=1, factor=0.5):
    for _ in range(times):
        bmesh.ops.smooth_vert(bm, verts=verts, factor=factor, use_axis_x=True, use_axis_y=True, use_axis_z=True)


def surface(verts, x, z, out=0.0):
    """Front surface point at (x, z), `out` metres towards the viewer."""
    from mathutils.bvhtree import BVHTree

    faces = list({f for v in verts for f in v.link_faces})
    tmp = bmesh.new()
    vmap = {}
    for f in faces:
        tmp.faces.new([vmap.setdefault(v, tmp.verts.new(v.co)) for v in f.verts])
    hit = BVHTree.FromBMesh(tmp).ray_cast(Vector((x, -2, z)), Vector((0, 1, 0)))[0]
    tmp.free()
    assert hit is not None, f"no surface at {x}, {z}"
    return hit - Vector((0, out, 0))


# --- body -----------------------------------------------------------------------

# Skin-modifier skeleton: (position, (radius x, radius y)) and edges.
def skeleton():
    pts = [
        ((0, 0.01, 0.93), (0.115, 0.085)),  # 0 pelvis (root)
        ((0, 0.012, 1.06), (0.092, 0.068)),  # 1 waist
        ((0, 0.0, 1.22), (0.112, 0.078)),  # 2 chest
        ((0, 0.01, 1.33), (0.108, 0.068)),  # 3 upper chest
        ((0, 0.018, 1.42), (0.046, 0.044)),  # 4 neck base
        ((0, 0.02, 1.53), (0.039, 0.039)),  # 5 neck top
    ]
    edges = [(0, 1), (1, 2), (2, 3), (3, 4), (4, 5)]

    def chain(start, items):
        prev = start
        for p in items:
            pts.append(p)
            edges.append((prev, len(pts) - 1))
            prev = len(pts) - 1
        return prev

    for sx in (1, -1):
        wrist = chain(3, [((0.15 * sx, 0.012, 1.365), (0.044, 0.042)), ((0.215 * sx, 0.012, 1.11), (0.031, 0.031)), ((0.24 * sx, 0.0, 0.88), (0.021, 0.018))])
        palm = chain(wrist, [((0.243 * sx, -0.004, 0.82), (0.011, 0.025))])
        chain(palm, [((0.246 * sx, -0.006, 0.755), (0.008, 0.019))])
        chain(palm, [((0.234 * sx, -0.03, 0.83), (0.008, 0.008))])
        ankle = chain(0, [((0.085 * sx, 0.01, 0.87), (0.074, 0.074)), ((0.08 * sx, 0.005, 0.49), (0.044, 0.046)), ((0.078 * sx, 0.014, 0.35), (0.044, 0.047)), ((0.075 * sx, 0.012, 0.09), (0.027, 0.029))])
        chain(ankle, [((0.075 * sx, -0.1, 0.03), (0.03, 0.02))])
        chain(ankle, [((0.075 * sx, 0.035, 0.035), (0.026, 0.022))])
    return pts, edges


def base_body(scene, arm):
    """The body as a subdivided skin-modifier mesh, sculpted, heat-weighted.
    Returns (positions, normals, faces, weights per vertex)."""
    pts, edges = skeleton()
    me = bpy.data.meshes.new("v_body_base")
    me.from_pydata([p for p, _ in pts], edges, [])
    obj = bpy.data.objects.new("v_body_base", me)
    scene.collection.objects.link(obj)
    obj.modifiers.new("skin", "SKIN")
    for i, (_, r) in enumerate(pts):
        me.skin_vertices[0].data[i].radius = r
    me.skin_vertices[0].data[0].use_root = True
    sub = obj.modifiers.new("sub", "SUBSURF")
    sub.levels = sub.render_levels = 2
    vl = scene.view_layers[0]
    vl.update()
    shaped = bpy.data.meshes.new_from_object(obj.evaluated_get(vl.depsgraph))
    obj.modifiers.clear()
    obj.data = shaped
    bpy.data.meshes.remove(me)

    bm = bmesh.new()
    bm.from_mesh(shaped)
    vs = bm.verts[:]
    for sx in (1, -1):
        brush(vs, (0.055 * sx, -0.055, 1.235), 0.075, (0.006 * sx, -0.04, 0.004), (1, 1, 0.85))  # bust
        brush(vs, (0.06 * sx, 0.07, 0.87), 0.08, (0, 0.024, 0))  # seat
        brush(vs, (0.11 * sx, 0.0, 0.93), 0.1, (0.016 * sx, 0, 0))  # hips
        brush(vs, (0.1 * sx, 0.0, 1.07), 0.06, (-0.008 * sx, 0, 0))  # waist
        brush(vs, (0.078 * sx, 0.05, 0.33), 0.08, (0, 0.012, 0))  # calves
        brush(vs, (0.08 * sx, -0.04, 0.68), 0.12, (0.004 * sx, -0.006, 0))  # thighs
        brush(vs, (0.16 * sx, 0.01, 1.37), 0.05, (0.004 * sx, 0, 0.006))  # shoulders
        brush(vs, (0.05 * sx, -0.05, 1.33), 0.05, (0, -0.006, 0))  # collarbones
    relax(bm, vs, 2, 0.3)
    bm.to_mesh(shaped)
    bm.free()

    for o in (obj, arm):
        o.select_set(True, view_layer=vl)
    vl.objects.active = arm
    with ctx(scene, active_object=arm, object=arm, selected_objects=[obj, arm], selected_editable_objects=[obj, arm]):
        bpy.ops.object.parent_set(type="ARMATURE_AUTO")
    names = [g.name for g in obj.vertex_groups]
    shaped.calc_loop_triangles()
    data = (
        [v.co.copy() for v in shaped.vertices],
        [v.normal.copy() for v in shaped.vertices],
        [tuple(p.vertices) for p in shaped.polygons],
        [{names[g.group]: g.weight for g in v.groups if g.weight > 0.01} for v in shaped.vertices],
    )
    bpy.data.objects.remove(obj)
    bpy.data.meshes.remove(shaped)
    return data


def dominant(w):
    return max(w, key=w.get) if w else "hips"


def neckline(co):
    """Top edge of the bodice: a sweetheart dip in front, straight at the back."""
    if co.y < 0:
        return 1.285 + 0.028 * smoothstep(abs(co.x) / 0.06) - 0.012 * smoothstep(1 - abs(co.x) / 0.02)
    return 1.33


def add_body(b, scene, arm):
    pos, nrm, faces, weights = base_body(scene, arm)
    bm = b.bm
    verts = [bm.verts.new(p) for p in pos]
    for v, w in zip(verts, weights):
        for name, x in w.items():
            v[b.deform][b._index(b.groups, name)] = x
    centre = lambda f: sum((pos[i] for i in f), Vector()) / len(f)
    region = lambda f: dominant({k: sum(weights[i].get(k, 0) for i in f) for k in weights[f[0]]})

    def is_tights(f):
        r = region(f)
        return r.startswith(("thigh", "shin")) and centre(f).z > 0.1

    for f in faces:
        face = bm.faces.new([verts[i] for i in f])
        mi = b._index(b.mats, "tights" if is_tights(f) else "skin")
        face.material_index = mi
        face.smooth = True
        if is_tights(f):
            for loop in face.loops:
                p = loop.vert.co
                sx = 1 if p.x > 0 else -1
                loop[b.uv].uv = (math.atan2(p.y, p.x - 0.08 * sx) / (2 * math.pi) * 5, p.z * 18)

    # Clothes: shells lifted off the body along its normals.
    def shell(pick, offset, mat):
        chosen = [f for f in faces if pick(f)]
        made = {}
        for f in chosen:
            for i in f:
                if i not in made:
                    v = bm.verts.new(pos[i] + nrm[i] * offset)
                    for name, x in weights[i].items():
                        v[b.deform][b._index(b.groups, name)] = x
                    made[i] = v
            face = bm.faces.new([made[i] for i in f])
            face.material_index = b._index(b.mats, mat)
            face.smooth = True
        return list(made.values())

    torso = ("hips", "spine", "chest")
    bodice = shell(lambda f: region(f) in torso and 0.95 < centre(f).z < neckline(centre(f)) + 0.01, 0.005, "dress")
    for v in bodice:  # a clean top edge instead of the faces' zigzag
        if v.co.z > 1.2 and any(e.is_boundary for e in v.link_edges):
            v.co.z = neckline(v.co)
    sleeves = shell(lambda f: region(f).startswith(("upper_arm", "forearm")) and centre(f).z < 1.31, 0.004, "dress")
    for v in sleeves:  # a clean band around the upper arm
        if v.co.z > 1.27 and any(e.is_boundary for e in v.link_edges):
            v.co.z = 1.30
    boots = shell(lambda f: region(f).startswith(("shin", "foot")) and centre(f).z < 0.41, 0.007, "boots")
    relax(bm, boots, 1, 0.3)


def add_clothes(b):
    for s, sx in (("L", 1), ("R", -1)):
        # Flared lace cuffs, platform soles with a heel, a buckle strap.
        b.loft([((0.239 * sx, -0.001, 0.905), 0.026, 0.026), ((0.241 * sx, -0.003, 0.875), 0.036, 0.034), ((0.242 * sx, -0.004, 0.86), 0.042, 0.04)], "dress", f"forearm_{s}", segs=16, caps=False)
        b.box((0.075 * sx, -0.035, 0.022), (0.105, 0.23, 0.044), "sole", f"foot_{s}")
        b.box((0.075 * sx, 0.045, 0.05), (0.07, 0.07, 0.03), "sole", f"foot_{s}")
        b.loft([((0.077 * sx, 0.012, 0.34), 0.057, 0.06), ((0.077 * sx, 0.012, 0.355), 0.057, 0.06)], "silver", f"shin_{s}", segs=16, caps=False)

    # The skirt: flared, with folds deepening towards the hem. Rigid at the
    # waist, following the thighs lower down (more in front) so she can
    # walk and sit.
    def weights(co):
        front = 0.9 if co.y < 0 else 0.45
        t = smoothstep((0.97 - co.z) / 0.45) * front
        return {"hips": 1 - t, ("thigh_L" if co.x > 0 else "thigh_R"): t}

    segs = 64
    profile = [(0.99, 0.104, 0.078), (0.95, 0.135, 0.105), (0.88, 0.17, 0.14), (0.78, 0.205, 0.175), (0.66, 0.24, 0.21), (0.56, 0.262, 0.232)]
    rings = []
    for k in range(len(profile) - 1):
        for j in range(3):
            t = j / 3
            a, c = profile[k], profile[k + 1]
            rings.append(tuple(a[i] + (c[i] - a[i]) * t for i in range(3)))
    rings.append(profile[-1])

    def ring_verts(z, rx, ry, extra=0.0):
        depth = smoothstep((0.99 - z) / 0.4)
        out = []
        for i in range(segs):
            a = 2 * math.pi * i / segs
            fold = 1 + 0.05 * depth * math.sin(10 * a) + extra
            out.append(Vector(((rx * fold) * math.cos(a), 0.008 + (ry * fold) * math.sin(a), z)))
        return out

    bm = b.bm
    grid = [[bm.verts.new(p) for p in ring_verts(*r)] for r in rings]
    hem = [[bm.verts.new(p) for p in ring_verts(z, *profile[-1][1:], extra=e)] for z, e in ((0.56, 0.0), (0.535, 0.012))]
    for rows, mat in ((grid, "dress"), ([grid[-1]] + hem[1:], "hem")):
        for k in range(len(rows) - 1):
            for i in range(segs):
                j = (i + 1) % segs
                f = bm.faces.new((rows[k][i], rows[k + 1][i], rows[k + 1][j], rows[k][j]))
                f.material_index = b._index(b.mats, mat)
                f.smooth = True
    for v in [v for row in grid + hem for v in row]:
        for name, x in weights(v.co).items():
            if x > 0:
                v[b.deform][b._index(b.groups, name)] = x
    for row in hem[:1]:
        for v in row:
            bm.verts.remove(v)
    # A violet sash at the waist, a choker with a pendant.
    b.loft([((0, 0.008, 0.975), 0.108, 0.082), ((0, 0.008, 1.0), 0.104, 0.079)], "hem", "hips", segs=32, caps=False)
    b.loft([((0, 0.02, 1.455), 0.044, 0.042), ((0, 0.02, 1.475), 0.043, 0.041)], "dress", "neck", segs=24, caps=False)
    b.ellipsoid((0, -0.024, 1.447), (0.006, 0.003, 0.008), "silver", "neck", segs=10, rings=8)


# --- head -----------------------------------------------------------------------


def add_head(b):
    """Returns the head's vertices (for the mouth_open key). Sculpted in its
    own bmesh, then merged in."""
    bm = bmesh.new()
    m = Matrix.Translation(HEAD_C) @ Matrix.Diagonal((*HEAD_R, 1))
    head = bmesh.ops.create_icosphere(bm, subdivisions=6, radius=1.0, matrix=m)["verts"]
    c = HEAD_C
    for v in head:
        p = v.co - c
        if p.z < 0:  # jaw and chin
            t = -p.z / HEAD_R.z
            p.x *= 1 - 0.34 * t * t
            if p.y < 0:
                p.y *= 1 - 0.12 * t
        if p.y < -0.05:  # a flatter face
            p.y = -0.05 + (p.y + 0.05) * 0.8
        v.co = c + p
    face = [v for v in head if v.co.y < c.y]
    s = lambda x, z, out=0.0: surface(face, x, c.z + z, out)
    brush(head, s(0, -0.098), 0.03, (0, -0.007, -0.003))  # chin
    for sx in (1, -1):
        brush(head, s(0.05 * sx, -0.01) + Vector((0, 0.01, 0)), 0.032, (0.004 * sx, -0.006, 0.002))  # cheekbones
        brush(head, s(0.045 * sx, -0.055) + Vector((0, 0.01, 0)), 0.03, (-0.004 * sx, 0.004, 0))  # hollow cheeks
        brush(head, s(0.035 * sx, 0.035), 0.028, (0, -0.004, 0.001))  # brow ridge
        brush(head, s(0.033 * sx, 0.01), 0.025, (0, 0.007, 0), (1.2, 1, 1))  # eye sockets
    brush(head, s(0, 0.0), 0.016, (0, -0.007, 0), (1, 1, 2.2))  # nose bridge
    brush(head, s(0, -0.028), 0.014, (0, -0.011, 0.002))  # nose tip
    for sx in (1, -1):
        brush(head, s(0.01 * sx, -0.032), 0.008, (0.0015 * sx, -0.002, 0))  # nostrils
    mz = MOUTH_Z - c.z
    brush(head, s(0, mz + 0.008), 0.017, (0, -0.006, 0), (1.7, 1, 0.55))  # upper lip
    brush(head, s(0, mz - 0.008), 0.016, (0, -0.007, 0), (1.5, 1, 0.6))  # lower lip
    for sx in (1, -1):
        brush(head, s(0.023 * sx, mz), 0.008, (0, 0.003, 0))  # mouth corners
    relax(bm, head, 1, 0.25)
    front = s(0, mz)

    # The mouth: cut the lips along a straight line and split it, so the
    # jaw can open it; a dark mouth inside.
    near = list({f for v in head if abs(v.co.z - MOUTH_Z) < 0.01 and abs(v.co.x) < 0.03 and v.co.y < c.y - 0.04 for f in v.link_faces})
    geom = list({e for f in near for e in f.edges} | set(near) | {v for f in near for v in f.verts})
    cut = bmesh.ops.bisect_plane(bm, geom=geom, plane_co=(0, 0, MOUTH_Z), plane_no=(0, 0, 1))["geom_cut"]
    seam = [e for e in cut if isinstance(e, bmesh.types.BMEdge) and all(abs(v.co.x) < 0.021 and v.co.y < c.y - 0.04 for v in e.verts)]
    bmesh.ops.split_edges(bm, edges=seam)
    head = b.merge(bm, "skin", "head")
    lips = [f for f in {f for v in head for f in v.link_faces} if f.calc_center_median().y < c.y - 0.05 and ((f.calc_center_median().x / 0.024) ** 2 + ((f.calc_center_median().z - MOUTH_Z + 0.001) / 0.011) ** 2) < 1]
    for f in lips:
        f.material_index = b._index(b.mats, "lips")
    b.ellipsoid(front + Vector((0, 0.016, 0)), (0.02, 0.012, 0.013), "mouth", "head", segs=16, rings=10)

    # Eyes in the sockets, with a lash line and winged liner; brows.
    for sx in (1, -1):
        socket = s(0.033 * sx, 0.01)
        eye = socket + Vector((0, 0.011, 0))
        b.ellipsoid(eye, (0.0145, 0.0145, 0.0145), "eye", "head", segs=24, rings=16, rot=(math.pi / 2, 0, 0))
        b.ellipsoid(eye + Vector((0.001 * sx, -0.0115, 0.0095)), (0.016, 0.004, 0.0024), "liner", "head", segs=12, rings=6, rot=(0.35, -0.15 * sx, 0))
        b.ellipsoid(eye + Vector((0.018 * sx, -0.008, 0.007)), (0.007, 0.002, 0.0018), "liner", "head", segs=8, rings=6, rot=(0, -0.55 * sx, 0))
        b.ellipsoid(s(0.036 * sx, 0.036, 0.001), (0.017, 0.003, 0.0024), "liner", "head", segs=12, rings=6, rot=(0, 0.12 * sx, 0))
    bm.free()
    return head


def add_hair(b):
    # Built apart: solidify invalidates every vertex reference in its bmesh.
    bm = bmesh.new()
    c = HEAD_C + Vector((0, 0.008, 0.012))
    r = HEAD_R + Vector((0.013, 0.013, 0.013))
    hair = bmesh.ops.create_icosphere(bm, subdivisions=5, radius=1.0, matrix=Matrix.Translation(c) @ Matrix.Diagonal((*r, 1)))["verts"]
    bang = HEAD_C.z + 0.052
    cut = [v for v in hair if v.co.z < HEAD_C.z - 0.105 or (v.co.y < HEAD_C.y - 0.03 and v.co.z < bang and abs(v.co.x) < 0.071)]
    bmesh.ops.delete(bm, geom=cut, context="VERTS")
    hair = [v for v in hair if v.is_valid]
    for v in hair:
        p = v.co - c
        a = math.atan2(p.x, p.y)
        if v.co.z < HEAD_C.z:  # the bob flares out a little, then tucks in at the ends
            t = (HEAD_C.z - v.co.z) / 0.105
            f = 1 + 0.16 * math.sin(t * math.pi * 0.8)
            p.x *= f
            p.y = p.y * f if p.y > 0 else p.y * (1 + 0.5 * (f - 1))
        # Strand grooves, deeper towards the ends.
        depth = 0.0025 * smoothstep((c.z + 0.08 - v.co.z) / 0.12)
        g = depth * math.sin(a * 44)
        n = Vector((p.x, p.y, 0)).normalized() if p.xy.length > 1e-6 else Vector()
        v.co = c + p + n * g
        # A blunt, slightly jagged fringe.
        if v.co.y < HEAD_C.y - 0.02 and abs(v.co.z - bang) < 0.012:
            v.co.z -= 0.003 * (0.5 + 0.5 * math.sin(v.co.x * 520))
    faces = list({f for v in hair for f in v.link_faces})
    bmesh.ops.solidify(bm, geom=faces, thickness=0.009)
    b.merge(bm, "hair", "head")
    bm.free()


def armature(scene):
    arm = bpy.data.objects.new("vesper_rig", bpy.data.armatures.new("vesper_rig"))
    scene.collection.objects.link(arm)
    scene.view_layers[0].objects.active = arm
    with ctx(scene, active_object=arm, object=arm, selected_objects=[arm]):
        bpy.ops.object.mode_set(mode="EDIT")
        for name, (h, t, parent) in BONES.items():
            e = arm.data.edit_bones.new(name)
            e.head, e.tail = h, t
            if parent:
                e.parent = arm.data.edit_bones[parent]
                e.use_connect = False
        bpy.ops.object.mode_set(mode="OBJECT")
    return arm



def build(scene):
    mats = materials()
    arm = armature(scene)
    b = Builder()
    add_body(b, scene, arm)
    add_clothes(b)
    head = add_head(b)
    add_hair(b)
    b.bm.verts.index_update()
    # mouth_open: the jaw turns down about a hinge below the ears.
    hinge = Vector((0, HEAD_C.y + 0.02, MOUTH_Z + 0.012))
    angle = math.radians(9)
    jaw = []
    for v in head:
        p = v.co
        if abs(p.z - MOUTH_Z) < 1e-5:  # on the split seam: the lower lip's copy opens fully
            below = sum(f.calc_center_median().z for f in v.link_faces) / max(1, len(v.link_faces)) < MOUTH_Z
            wz = 1.0 if below else 0.0
        else:
            wz = 1.0 if p.z < MOUTH_Z else 0.0
        w = wz * (1 - smoothstep((abs(p.x) - 0.03) / 0.04)) * (1 - smoothstep((p.y - HEAD_C.y + 0.02) / 0.05))
        if w > 0:
            rot = Matrix.Rotation(angle * w, 3, "X")
            jaw.append((v.index, hinge + rot @ (p - hinge)))
    mesh = b.object("vesper", scene.collection, mats)
    mesh.parent = arm
    mesh.modifiers.new("rig", "ARMATURE").object = arm
    mesh.shape_key_add(name="Basis")
    key = mesh.shape_key_add(name="mouth_open", from_mix=False)
    for i, co in jaw:
        key.data[i].co = co
    clips(scene, arm)
    return scene


# --- animation -----------------------------------------------------------------


def q_for(arm, bone, rot):
    """A rotation given in armature axes (degrees, XYZ), relative to the
    parent, as the bone's local pose rotation."""
    rest = arm.data.bones[bone].matrix_local.to_quaternion()
    q = Euler([math.radians(a) for a in rot], "XYZ").to_quaternion()
    return rest.conjugated() @ q @ rest


def loc_for(arm, bone, loc):
    rest = arm.data.bones[bone].matrix_local.to_quaternion()
    return rest.conjugated() @ Vector(loc)


# Front is -Y: rotating about +X by a negative angle swings a limb that
# hangs down forwards; a positive angle bends the spine/head forwards.
# About Y, a negative angle lifts the left arm sideways, positive the right.
RELAX = {"upper_arm_L": (0, -4, 0), "upper_arm_R": (0, 4, 0), "forearm_L": (-6, 0, 0), "forearm_R": (-6, 0, 0)}


def pose(**over):
    p = dict(RELAX)
    p.update(over)
    return p


def walk_pose(phase):
    """phase 0..1 over one full stride (two steps)."""
    s = math.sin(2 * math.pi * phase)
    c = math.cos(2 * math.pi * phase)
    lift = lambda x: max(0.0, x)
    return pose(
        hips=(0, 0, 6 * s),
        spine=(4, 0, -4 * s),
        chest=(0, 0, -5 * s),
        head=(-3, 0, 3 * s),
        thigh_L=(-26 * s, 0, 0),
        thigh_R=(26 * s, 0, 0),
        shin_L=(8 + 40 * lift(c), 0, 0),
        shin_R=(8 + 40 * lift(-c), 0, 0),
        foot_L=(-10 * s, 0, 0),
        foot_R=(10 * s, 0, 0),
        upper_arm_L=(20 * s, -6, 0),
        upper_arm_R=(-20 * s, 6, 0),
        forearm_L=(-15 - 10 * lift(s), 0, 0),
        forearm_R=(-15 - 10 * lift(-s), 0, 0),
        _hips_loc=(0, 0, 0.02 * abs(c) - 0.01),
    )


def sit_pose(**over):
    return pose(
        thigh_L=(-88, 0, -4),
        thigh_R=(-88, 0, 4),
        shin_L=(86, 0, 0),
        shin_R=(86, 0, 0),
        foot_L=(2, 0, 0),
        foot_R=(2, 0, 0),
        spine=(-6, 0, 0),
        upper_arm_L=(-8, -2, 0),
        upper_arm_R=(-8, 2, 0),
        forearm_L=(-52, 0, -38),
        forearm_R=(-52, 0, 38),
        _hips_loc=(0, 0.05, -0.47),
        **over,
    )


def clip_defs():
    idle = lambda t: pose(chest=(1.5 * math.sin(2 * math.pi * t), 0, 0), hips=(0, 1.5 * math.sin(2 * math.pi * t), 0), head=(0, 2 * math.sin(2 * math.pi * t + 1), 1.5 * math.sin(2 * math.pi * t)))
    talk = lambda t: pose(
        head=(3 * math.sin(4 * math.pi * t), 4 * math.sin(2 * math.pi * t), 5 * math.sin(2 * math.pi * t + 0.5)),
        upper_arm_R=(-18 - 8 * math.sin(2 * math.pi * t), 10, 0),
        forearm_R=(-50 - 15 * math.sin(4 * math.pi * t), 0, 25),
        chest=(2, 0, 3 * math.sin(2 * math.pi * t)),
    )

    def wave(t):
        up = min(1.0, t / 0.2, (1 - t) / 0.2)
        w = math.sin(2 * math.pi * 3 * t) * 22
        return pose(upper_arm_R=(-10 * up, 4 + 92 * up, 0), forearm_R=(0, (80 + w) * up, 0), head=(0, -6 * up, -8 * up), hand_R=(0, 0, 0))

    def reach(t):
        k = math.sin(math.pi * t)
        return pose(spine=(10 * k, 0, 0), upper_arm_R=(-70 * k, 4, 0), forearm_R=(-6 - 20 * k, 0, 0), head=(8 * k, 0, 0))

    def think(t):
        k = min(1.0, t / 0.2, (1 - t) / 0.2)
        return pose(
            upper_arm_R=(-45 * k, 4 - 18 * k, 0),
            forearm_R=(-6 - 128 * k, 0, 18 * k),
            upper_arm_L=(-25 * k, -4, 0),
            forearm_L=(-6 - 80 * k, 0, -35 * k),
            head=(-8 * k, -10 * k, 12 * k),
        )

    def nod(t):
        return pose(head=(16 * abs(math.sin(2 * math.pi * t)), 0, 0), neck=(4 * abs(math.sin(2 * math.pi * t)), 0, 0))

    def shrug(t):
        k = math.sin(math.pi * t)
        return pose(upper_arm_L=(-10 * k, -4 - 18 * k, 0), upper_arm_R=(-10 * k, 4 + 18 * k, 0), forearm_L=(-6 - 60 * k, 0, -30 * k), forearm_R=(-6 - 60 * k, 0, 30 * k), head=(0, 12 * k, 0), chest=(-3 * k, 0, 0))

    # name: (seconds, keys per second, pose(t in 0..1), loops)
    return {
        "idle": (4.0, 6, idle, True),
        "walk": (1.0, 16, walk_pose, True),
        "talk": (2.0, 10, talk, True),
        "sit": (4.0, 4, lambda t: sit_pose(chest=(1.5 * math.sin(2 * math.pi * t), 0, 0)), True),
        "wave": (2.0, 15, wave, False),
        "reach": (1.0, 12, reach, False),
        "think": (3.0, 8, think, False),
        "nod": (1.2, 15, nod, False),
        "shrug": (1.5, 12, shrug, False),
    }


def clips(scene, arm):
    ad = arm.animation_data_create()
    for pb in arm.pose.bones:
        pb.rotation_mode = "QUATERNION"
    defs = clip_defs()
    # Earlier builds' actions (kept by their fake users) would be exported too.
    for act in [a for a in bpy.data.actions if a.name.split(".")[0] in defs]:
        bpy.data.actions.remove(act)
    for name, (secs, rate, fn, _loops) in defs.items():
        act = bpy.data.actions.new(name)
        act.use_fake_user = True
        ad.action = act
        n = max(2, round(secs * rate))
        prev = {}
        for k in range(n + 1):
            t = k / n
            frame = t * secs * FPS
            p = fn(t)
            for pb in arm.pose.bones:
                q = q_for(arm, pb.name, p.get(pb.name, (0, 0, 0)))
                if pb.name in prev and prev[pb.name].dot(q) < 0:
                    q.negate()
                prev[pb.name] = q
                pb.rotation_quaternion = q
                pb.keyframe_insert("rotation_quaternion", frame=frame, group=pb.name)
            hips = arm.pose.bones["hips"]
            hips.location = loc_for(arm, "hips", p.get("_hips_loc", (0, 0, 0)))
            hips.keyframe_insert("location", frame=frame, group="hips")
        track = ad.nla_tracks.new()
        track.name = name
        track.strips.new(name, 0, act)
        track.mute = True
    ad.action = None
    for pb in arm.pose.bones:
        pb.rotation_quaternion = Quaternion()
        pb.location = Vector()


def previews(scene, out):
    arm = bpy.data.objects["vesper_rig"]
    ad = arm.animation_data
    shots = [("vesper_front", None, 0, (0.3, -2.6, 1.3), (0, 0, 1.0), 45),
             ("vesper_face", None, 0, (0.15, -0.9, 1.6), (0, 0, 1.58), 70),
             ("vesper_back", None, 0, (-0.8, 2.2, 1.4), (0, 0, 1.0), 45),
             ("vesper_walk", "walk", 8, (-1.8, -1.8, 1.2), (0, 0, 0.9), 45),
             ("vesper_wave", "wave", 30, (0.4, -2.4, 1.3), (0, 0, 1.1), 45),
             ("vesper_sit", "sit", 0, (-1.8, -1.6, 0.9), (0, 0, 0.6), 45),
             ("vesper_think", "think", 45, (0.3, -1.8, 1.4), (0, 0, 1.3), 50)]
    mesh = bpy.data.objects["vesper"]
    for name, clip, frame, eye, target, lens in shots:
        ad.action = bpy.data.actions[clip] if clip else None
        for pb in arm.pose.bones:
            pb.rotation_quaternion = Quaternion()
            pb.location = Vector()
        scene.frame_set(frame)
        mesh.data.shape_keys.key_blocks["mouth_open"].value = 1.0 if name == "vesper_face" else 0.0
        preview(scene, out / f"{name}.png", eye, target, lens)
    ad.action = None
    mesh.data.shape_keys.key_blocks["mouth_open"].value = 0.0
