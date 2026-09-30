"""Vesper: a stylized low-poly woman of about 25, blue bob with bangs, black
dress with a violet hem, fishnet tights, platform boots, a choker. One
skinned mesh (rigid parts, blended at the skirt), an armature, clips and a
`mouth_open` shape key on the lips.
"""

import math

import bpy
from mathutils import Euler, Quaternion, Vector

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

HEAD_C = Vector((0, 0, 1.60))


def materials():
    def fishnet(x, y):
        on = (x + y) % 8 == 0 or (x - y) % 8 == 0
        return (0.02, 0.02, 0.025) if on else (0.86, 0.74, 0.70)

    net = image("v_fishnet", (32, 32), fishnet)
    return {
        "skin": material("v_skin", (0.92, 0.80, 0.76), rough=0.55),
        "dress": material("v_dress", (0.025, 0.022, 0.03), rough=0.45),
        "hem": material("v_hem", (0.22, 0.05, 0.32), rough=0.5),
        "tights": material("v_tights", (1, 1, 1), rough=0.7, image=net),
        "boots": material("v_boots", (0.015, 0.015, 0.02), rough=0.25, metal=0.1),
        "sole": material("v_sole", (0.06, 0.06, 0.07), rough=0.8),
        "hair": material("v_hair", (0.09, 0.28, 0.82), rough=0.35, double=True),
        "eye": material("v_eye", (0.92, 0.92, 0.95), rough=0.2),
        "iris": material("v_iris", (0.22, 0.12, 0.42), rough=0.15),
        "liner": material("v_liner", (0.01, 0.01, 0.015), rough=0.4),
        "lips": material("v_lips", (0.24, 0.03, 0.12), rough=0.35),
        "silver": material("v_silver", (0.8, 0.8, 0.85), rough=0.2, metal=1.0),
    }


def body(b):
    # Head: an egg with a narrower jaw.
    head = b.ellipsoid(HEAD_C, (0.106, 0.112, 0.128), "skin", "head", segs=24, rings=16)
    for v in head:
        dz = HEAD_C.z - v.co.z
        if dz > 0:
            t = dz / 0.128
            v.co.x *= 1 - 0.32 * t * t
            v.co.y = HEAD_C.y + (v.co.y - HEAD_C.y) * (1 - 0.12 * t)
    b.ellipsoid((0, -0.108, 1.578), (0.009, 0.01, 0.014), "skin", "head", segs=8, rings=6)  # nose

    def on_face(x, z, out=0.0):
        p = b.surface(x, z, head)
        return Vector((p.x, p.y - out, p.z))

    for sx in (1, -1):
        e = on_face(0.041 * sx, 1.607)
        b.ellipsoid(e + Vector((0, 0.001, 0)), (0.023, 0.007, 0.017), "eye", "head", segs=14, rings=8)
        b.ellipsoid(e + Vector((0.001 * sx, -0.006, -0.002)), (0.011, 0.004, 0.013), "iris", "head", segs=12, rings=8)
        # Eyeliner along the upper lid ending in a wing, and an arched brow.
        b.ellipsoid(e + Vector((0.003 * sx, -0.006, 0.015)), (0.025, 0.004, 0.0035), "liner", "head", segs=10, rings=6, rot=(0, -0.12 * sx, 0))
        b.ellipsoid(e + Vector((0.027 * sx, -0.001, 0.017)), (0.009, 0.003, 0.0025), "liner", "head", segs=8, rings=6, rot=(0, -0.6 * sx, 0))
        b.ellipsoid(on_face(0.043 * sx, 1.643, 0.003), (0.022, 0.004, 0.0035), "liner", "head", segs=10, rings=6, rot=(0, 0.15 * sx, 0))
    # Upper and lower lip; the lower one drops for mouth_open.
    m = on_face(0, 1.548, 0.001)
    b.ellipsoid(m + Vector((0, 0, 0.0045)), (0.021, 0.006, 0.0055), "lips", "head", segs=14, rings=8)
    lips = b.ellipsoid(m + Vector((0, 0.001, -0.0045)), (0.019, 0.006, 0.0065), "lips", "head", segs=14, rings=8)

    # Hair: a shell around the head, open for the face, flaring into a bob,
    # bangs down to the brows.
    import bmesh

    hair = b.ellipsoid(HEAD_C + Vector((0, 0.012, 0.012)), (0.122, 0.13, 0.142), "hair", "head", segs=32, rings=20)
    cut = [v for v in hair if v.co.z < 1.50 or (v.co.y < -0.02 and v.co.z < 1.662 and abs(v.co.x) < 0.086)]
    bmesh.ops.delete(b.bm, geom=cut, context="VERTS")
    hair = [v for v in hair if v.is_valid]
    for v in hair:
        if v.co.z < 1.60:
            f = 1 + (1.60 - v.co.z) * 1.1
            v.co.x *= f
            v.co.y = 0.012 + (v.co.y - 0.012) * f

    # Neck and choker with a small silver pendant.
    b.loft([((0, 0.005, 1.38), 0.05, 0.045), ((0, 0.005, 1.53), 0.042, 0.042)], "skin", "neck", segs=12)
    b.loft([((0, 0.005, 1.455), 0.047, 0.046), ((0, 0.005, 1.475), 0.047, 0.046)], "dress", "neck", segs=16)
    b.ellipsoid((0, -0.043, 1.448), (0.007, 0.004, 0.009), "silver", "neck", segs=8, rings=6)

    # Torso: skin at the neckline, black dress below.
    b.loft([((0, 0.01, 1.30), 0.155, 0.085), ((0, 0.01, 1.36), 0.15, 0.08), ((0, 0.01, 1.40), 0.07, 0.05)], "skin", "chest")
    b.loft(
        [((0, 0.01, 0.97), 0.125, 0.085), ((0, 0.01, 1.08), 0.115, 0.08), ((0, 0.01, 1.2), 0.14, 0.09), ((0, 0.01, 1.31), 0.158, 0.088)],
        "dress",
        weigh=lambda co: {"spine": 1.0} if co.z < 1.2 else {"chest": 1.0},
    )
    for sx in (1, -1):
        b.ellipsoid((0.05 * sx, -0.045, 1.232), (0.055, 0.042, 0.046), "dress", "chest", segs=12, rings=8)

    # Skirt: flared, rigid on the hips at the waist, following the thighs
    # towards the hem so she can sit and walk.
    def skirt_weights(co):
        front = 0.9 if co.y < 0 else 0.45
        t = max(0.0, min(1.0, (0.97 - co.z) / 0.45)) * front
        side = "thigh_L" if co.x > 0 else "thigh_R"
        return {"hips": 1 - t, side: t}

    b.loft([((0, 0.01, 0.99), 0.128, 0.088), ((0, 0.01, 0.85), 0.19, 0.15), ((0, 0.0, 0.58), 0.25, 0.21)], "dress", segs=20, weigh=skirt_weights, caps=False)
    b.loft([((0, 0.0, 0.58), 0.25, 0.21), ((0, 0.0, 0.54), 0.255, 0.215)], "hem", segs=20, weigh=skirt_weights, caps=False)

    for s, sx in (("L", 1), ("R", -1)):
        # Arms in long sleeves with flared cuffs.
        b.ellipsoid((0.172 * sx, 0.005, 1.35), (0.046, 0.044, 0.044), "dress", f"upper_arm_{s}", segs=12, rings=8)
        b.loft([((0.18 * sx, 0, 1.36), 0.038, 0.038), ((0.215 * sx, 0, 1.11), 0.031, 0.031)], "dress", f"upper_arm_{s}", segs=10)
        b.ellipsoid((0.215 * sx, 0, 1.11), (0.031, 0.031, 0.031), "dress", f"forearm_{s}", segs=10, rings=6)
        b.loft([((0.215 * sx, 0, 1.11), 0.03, 0.03), ((0.235 * sx, -0.008, 0.92), 0.026, 0.026), ((0.24 * sx, -0.01, 0.885), 0.038, 0.038)], "dress", f"forearm_{s}", segs=10)
        b.loft([((0.24 * sx, -0.01, 0.9), 0.02, 0.02), ((0.242 * sx, -0.01, 0.84), 0.019, 0.02)], "skin", f"hand_{s}", segs=8)  # wrist
        b.ellipsoid((0.243 * sx, -0.01, 0.82), (0.02, 0.034, 0.055), "skin", f"hand_{s}", segs=10, rings=8)
        b.ellipsoid((0.233 * sx, -0.032, 0.84), (0.011, 0.011, 0.028), "skin", f"hand_{s}", segs=8, rings=6, rot=(0.3, 0, 0))  # thumb
        # Legs in fishnets, knee-high platform boots.
        b.loft([((0.085 * sx, 0, 0.87), 0.068, 0.068), ((0.082 * sx, 0, 0.7), 0.066, 0.066), ((0.08 * sx, 0, 0.49), 0.05, 0.05)], "tights", f"thigh_{s}", segs=12)
        b.ellipsoid((0.08 * sx, 0, 0.49), (0.05, 0.05, 0.05), "tights", f"shin_{s}", segs=10, rings=6)
        b.loft([((0.08 * sx, 0, 0.49), 0.049, 0.049), ((0.078 * sx, 0.004, 0.40), 0.047, 0.047)], "tights", f"shin_{s}", segs=12)
        b.loft([((0.078 * sx, 0.004, 0.41), 0.056, 0.056), ((0.077 * sx, 0.008, 0.25), 0.05, 0.05), ((0.075 * sx, 0.01, 0.09), 0.042, 0.044)], "boots", f"shin_{s}", segs=12)
        b.ellipsoid((0.075 * sx, -0.035, 0.085), (0.048, 0.1, 0.048), "boots", f"foot_{s}", segs=12, rings=8)
        b.box((0.075 * sx, -0.035, 0.022), (0.1, 0.22, 0.045), "sole", f"foot_{s}")
        b.loft([((0.077 * sx, 0.004, 0.35), 0.058, 0.058), ((0.077 * sx, 0.004, 0.365), 0.058, 0.058)], "silver", f"shin_{s}", segs=12)  # buckle strap
    return lips


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
    b = Builder()
    lips = body(b)
    lip_c = sum((v.co for v in lips), Vector()) / len(lips)
    b.bm.verts.index_update()
    lip_ids = [(v.index, v.co.copy()) for v in lips]
    mesh = b.object("vesper", scene.collection, mats)
    arm = armature(scene)
    mesh.parent = arm
    mesh.modifiers.new("rig", "ARMATURE").object = arm

    # mouth_open: the lower lip stretches down and narrows a little.
    mesh.shape_key_add(name="Basis")
    key = mesh.shape_key_add(name="mouth_open", from_mix=False)
    for i, co in lip_ids:
        d = co - lip_c
        drop = 0.011 if d.z < 0 else 0.002
        key.data[i].co = Vector((lip_c.x + d.x * 0.85, co.y, co.z - drop * (1 - 0.5 * abs(d.x) / 0.019)))

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
