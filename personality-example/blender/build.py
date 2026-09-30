"""Builds Vesper's assets: assets/vesper.glb (rigged character with clips and
a mouth_open shape key), assets/props.glb (the things in the room, one
top-level node per world object id) and assets/room.glb (floor and walls).

Headless (reproducible):
    blender --background --factory-startup --python blender/build.py -- assets
In the running Blender, through the Blender MCP add-on:
    python3 blender/live.py blender/build.py assets [--preview DIR]

Everything is built in its own scenes ("Vesper", "Room"), so an open file
is left alone. Coordinates: Blender's (z up, the character faces -Y). The
glTF exporter turns them into y up, facing +z, which is the world's.
"""

import math
import sys
from pathlib import Path

import bmesh
import bpy
from mathutils import Euler, Matrix, Vector

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))


# --- context -----------------------------------------------------------------


def window():
    wm = bpy.context.window_manager
    return bpy.context.window or (wm.windows[0] if wm and wm.windows else None)


def fresh_scene(name):
    """A new empty scene (replacing an earlier build's) with its own collection."""
    old = bpy.data.scenes.get(name)
    if old:
        for o in list(old.objects):
            bpy.data.objects.remove(o, do_unlink=True)
        win = window()
        if win and win.scene == old:
            win.scene = next(s for s in bpy.data.scenes if s != old) if len(bpy.data.scenes) > 1 else bpy.data.scenes.new("Scene")
        bpy.data.scenes.remove(old)
    bpy.data.orphans_purge(do_recursive=True)
    scene = bpy.data.scenes.new(name)
    scene.render.fps = 30
    return scene


def ctx(scene, **kw):
    """A context override for operators, with or without a window."""
    win = window()
    if win:
        win.scene = scene
        kw["window"] = win
    return bpy.context.temp_override(scene=scene, view_layer=scene.view_layers[0], **kw)


# --- materials ---------------------------------------------------------------


def material(name, color, rough=0.6, metal=0.0, emit=None, alpha=None, image=None, double=False):
    m = bpy.data.materials.new(name)
    if hasattr(m, "use_nodes") and not m.use_nodes:
        m.use_nodes = True
    bsdf = m.node_tree.nodes["Principled BSDF"]
    bsdf.inputs["Base Color"].default_value = (*color, 1.0)
    bsdf.inputs["Roughness"].default_value = rough
    bsdf.inputs["Metallic"].default_value = metal
    if emit:
        bsdf.inputs["Emission Color"].default_value = (*emit, 1.0)
        bsdf.inputs["Emission Strength"].default_value = 2.0
    if alpha is not None:
        bsdf.inputs["Alpha"].default_value = alpha
        m.surface_render_method = "BLENDED"
    if image:
        tex = m.node_tree.nodes.new("ShaderNodeTexImage")
        tex.image = image
        tex.interpolation = "Closest" if image.size[0] <= 64 else "Linear"
        m.node_tree.links.new(tex.outputs["Color"], bsdf.inputs["Base Color"])
    m.use_backface_culling = not double
    return m


def image(name, size, pixel):
    """A packed image from pixel(x, y) -> (r, g, b)."""
    w, h = size
    img = bpy.data.images.new(name, w, h)
    px = []
    for y in range(h):
        for x in range(w):
            px.extend((*pixel(x, y), 1.0))
    img.pixels = px
    img.pack()
    return img


# --- meshes ------------------------------------------------------------------


class Builder:
    """Collects parts into one bmesh, each with a material and bone weights."""

    def __init__(self):
        self.bm = bmesh.new()
        self.uv = self.bm.loops.layers.uv.verify()
        self.deform = self.bm.verts.layers.deform.verify()
        self.mats = []
        self.groups = []

    def _index(self, lst, item):
        if item not in lst:
            lst.append(item)
        return lst.index(item)

    def add(self, verts, mat, bone=None, weigh=None):
        faces = {f for v in verts for f in v.link_faces}
        mi = self._index(self.mats, mat)
        for f in faces:
            f.material_index = mi
            f.smooth = True
        if bone or weigh:
            for v in verts:
                for b, w in (weigh(v.co) if weigh else {bone: 1.0}).items():
                    if w > 0:
                        v[self.deform][self._index(self.groups, b)] = w
        return verts

    def ellipsoid(self, c, r, mat, bone=None, segs=16, rings=10, rot=(0, 0, 0), weigh=None):
        m = Matrix.Translation(c) @ Euler(rot).to_matrix().to_4x4() @ Matrix.Diagonal((*r, 1))
        v = bmesh.ops.create_uvsphere(self.bm, u_segments=segs, v_segments=rings, radius=1.0, matrix=m, calc_uvs=True)["verts"]
        return self.add(v, mat, bone, weigh)

    def box(self, c, size, mat, bone=None, rot=(0, 0, 0)):
        m = Matrix.Translation(c) @ Euler(rot).to_matrix().to_4x4() @ Matrix.Diagonal((*size, 1))
        v = bmesh.ops.create_cube(self.bm, size=1.0, matrix=m, calc_uvs=True)["verts"]
        verts = self.add(v, mat, bone)
        for f in {f for v in verts for f in v.link_faces}:
            f.smooth = False
        return verts

    def cylinder(self, c, r1, r2, depth, mat, bone=None, segs=16, rot=(0, 0, 0), smooth=True):
        """Centred at c, along z (before rot)."""
        m = Matrix.Translation(c) @ Euler(rot).to_matrix().to_4x4()
        v = bmesh.ops.create_cone(self.bm, cap_ends=True, segments=segs, radius1=r1, radius2=r2, depth=depth, matrix=m, calc_uvs=True)["verts"]
        verts = self.add(v, mat, bone)
        if not smooth:
            for f in {f for v in verts for f in v.link_faces}:
                f.smooth = False
        return verts

    def loft(self, rings, mat, bone=None, segs=14, weigh=None, caps=True):
        """Rings of (centre, rx, ry) stacked into a tube, with UVs around/along."""
        bm = self.bm
        grid = []
        for c, rx, ry in rings:
            c = Vector(c)
            grid.append([bm.verts.new(c + Vector((rx * math.cos(a), ry * math.sin(a), 0))) for a in (2 * math.pi * i / segs for i in range(segs))])
        faces = []
        for k in range(len(grid) - 1):
            for i in range(segs):
                j = (i + 1) % segs
                f = bm.faces.new((grid[k][i], grid[k][j], grid[k + 1][j], grid[k + 1][i]))
                for loop, (u, v) in zip(f.loops, ((i, k), (i + 1, k), (i + 1, k + 1), (i, k + 1))):
                    loop[self.uv].uv = (u / segs, v / (len(grid) - 1))
                faces.append(f)
        if caps:
            bm.faces.new(list(reversed(grid[0])))
            bm.faces.new(grid[-1])
        bmesh.ops.recalc_face_normals(bm, faces=list({f for ring in grid for v in ring for f in v.link_faces}))
        return self.add([v for ring in grid for v in ring], mat, bone, weigh)

    def surface(self, x, z, verts):
        """Where a ray from the front (-Y) at (x, z) hits the given part."""
        from mathutils.bvhtree import BVHTree

        faces = list({f for v in verts for f in v.link_faces})
        tmp = bmesh.new()
        vmap = {}
        for f in faces:
            vs = []
            for v in f.verts:
                if v not in vmap:
                    vmap[v] = tmp.verts.new(v.co)
                vs.append(vmap[v])
            tmp.faces.new(vs)
        hit, *_ = BVHTree.FromBMesh(tmp).ray_cast(Vector((x, -2, z)), Vector((0, 1, 0)))
        tmp.free()
        return hit

    def object(self, name, collection, mats):
        me = bpy.data.meshes.new(name)
        self.bm.normal_update()
        self.bm.to_mesh(me)
        self.bm.free()
        for m in self.mats:
            me.materials.append(mats[m])
        obj = bpy.data.objects.new(name, me)
        collection.objects.link(obj)
        for g in self.groups:
            obj.vertex_groups.new(name=g)
        return obj


# --- export ------------------------------------------------------------------


def export(scene, path, roots=None, animations=False):
    """Exports the scene, or only `roots` and their children."""
    path.parent.mkdir(parents=True, exist_ok=True)
    vl = scene.view_layers[0]
    keep = set()
    for r in roots or scene.objects:
        keep |= {r, *r.children_recursive}
    for o in scene.objects:
        o.select_set(o in keep, view_layer=vl)
    with ctx(scene):
        bpy.ops.export_scene.gltf(
            filepath=str(path),
            export_format="GLB",
            use_active_scene=True,
            use_selection=True,
            export_apply=False,
            export_yup=True,
            export_skins=animations,
            export_morph=True,
            export_animations=animations,
            export_animation_mode="ACTIONS",
            export_force_sampling=True,
            export_optimize_animation_size=False,
            export_cameras=False,
            export_lights=False,
        )
    print(f"wrote {path} ({path.stat().st_size // 1024} KiB)")


def preview(scene, path, eye, target, lens=40):
    """A quick render to look at while developing."""
    cam = bpy.data.objects.new("preview_cam", bpy.data.cameras.new("preview_cam"))
    scene.collection.objects.link(cam)
    cam.data.lens = lens
    cam.location = eye
    cam.rotation_euler = (Vector(target) - Vector(eye)).to_track_quat("-Z", "Y").to_euler()
    sun = bpy.data.objects.new("preview_sun", bpy.data.lights.new("preview_sun", "SUN"))
    sun.data.energy = 3.0
    sun.rotation_euler = (math.radians(50), 0, math.radians(-30))
    scene.collection.objects.link(sun)
    world = bpy.data.worlds.new("preview_world")
    world.color = (0.2, 0.2, 0.22)
    scene.world = world
    scene.camera = cam
    scene.render.engine = "BLENDER_EEVEE"
    scene.render.resolution_x, scene.render.resolution_y = 900, 900
    scene.render.filepath = str(path)
    with ctx(scene):
        bpy.ops.render.render(write_still=True)
    bpy.data.objects.remove(cam)
    bpy.data.objects.remove(sun)
    print(f"preview {path}")


def main():
    argv = sys.argv[sys.argv.index("--") + 1 :] if "--" in sys.argv else sys.argv[1:]
    out = Path(argv[0] if argv else HERE.parent / "assets").resolve()
    previews = Path(argv[argv.index("--preview") + 1]).resolve() if "--preview" in argv else None

    # The running Blender keeps modules between runs.
    for m in ("build", "vesper", "props"):
        sys.modules.pop(m, None)
    import props
    import vesper

    scene = vesper.build(fresh_scene("Vesper"))
    export(scene, out / "vesper.glb", animations=True)
    if previews:
        vesper.previews(scene, previews)
    room = fresh_scene("Room")
    shell, things = props.build(room)
    export(room, out / "room.glb", [shell])
    export(room, out / "props.glb", list(things.values()))
    if previews:
        props.preview_all(room, previews)


if __name__ == "__main__":
    main()
