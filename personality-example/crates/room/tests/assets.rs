//! The committed Blender exports (assets/*.glb) have what the browser and
//! the world expect.

use std::collections::BTreeSet;
use std::path::PathBuf;

use vesper_room::world::World;

fn load(name: &str) -> gltf::Document {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets").join(name);
    gltf::Gltf::open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())).document
}

fn names<'a>(nodes: impl Iterator<Item = gltf::Node<'a>>) -> BTreeSet<String> {
    nodes.filter_map(|n| n.name().map(String::from)).collect()
}

#[test]
fn vesper_is_rigged_with_clips_and_a_mouth() {
    let doc = load("vesper.glb");
    let clips: BTreeSet<_> = doc.animations().filter_map(|a| a.name().map(String::from)).collect();
    let want: BTreeSet<String> = ["idle", "walk", "talk", "wave", "reach", "sit", "think", "nod", "shrug"].map(String::from).into();
    assert_eq!(clips, want);
    let mesh_node = doc.nodes().find(|n| n.name() == Some("vesper")).expect("vesper mesh node");
    let skin = mesh_node.skin().expect("skinned");
    let joints = names(skin.joints());
    for j in ["hips", "head", "hand_R", "foot_L"] {
        assert!(joints.contains(j), "joint {j} in {joints:?}");
    }
    let mesh = mesh_node.mesh().unwrap();
    let extras = mesh.extras().as_ref().expect("mesh extras with target names").get();
    assert!(extras.contains("\"mouth_open\""), "{extras}");
    assert!(mesh.primitives().all(|p| p.morph_targets().count() == 1));
    // About 1.7 m tall (y up), standing on the floor, facing +z.
    let (lo, hi) = mesh.primitives().fold(([f32::MAX; 3], [f32::MIN; 3]), |(lo, hi), p| {
        let b = p.bounding_box();
        (std::array::from_fn(|i| lo[i].min(b.min[i])), std::array::from_fn(|i| hi[i].max(b.max[i])))
    });
    assert!(lo[1].abs() < 0.02 && (1.65..1.85).contains(&hi[1]), "height {lo:?}..{hi:?}");
    for a in doc.animations() {
        assert!(a.channels().count() >= 17, "{} animates every bone", a.name().unwrap());
    }
}

#[test]
fn props_are_the_worlds_objects() {
    let doc = load("props.glb");
    let scene = doc.default_scene().or_else(|| doc.scenes().next()).unwrap();
    let top = names(scene.nodes());
    let world: BTreeSet<String> = World::new().objects.iter().map(|o| o.id.clone()).collect();
    assert_eq!(top, world);
    let all = names(doc.nodes());
    for part in ["pot", "coffee", "led", "mug_fill", "platter", "shade", "flame", "glass"] {
        assert!(all.contains(part), "{part} in {all:?}");
    }
}

#[test]
fn the_room_has_a_sky_behind_the_window() {
    let doc = load("room.glb");
    let all = names(doc.nodes());
    assert!(all.contains("room") && all.contains("sky"), "{all:?}");
}
