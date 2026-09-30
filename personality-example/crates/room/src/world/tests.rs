use super::*;

fn walk(w: &mut World) -> Vec<Event> {
    let mut events = vec![];
    for _ in 0..2000 {
        let t = w.t + 0.05;
        events.extend(w.tick(t));
        if !w.walking() {
            break;
        }
    }
    assert!(!w.walking(), "never arrived");
    events
}

fn go(w: &mut World, id: &str) {
    w.move_to(&Target::Thing(id.into())).unwrap();
    walk(w);
}

fn thing(id: &str) -> Target {
    Target::Thing(id.into())
}

#[test]
fn targets_parse() {
    assert_eq!(Target::parse(&json!("mug")).unwrap(), thing("mug"));
    assert_eq!(Target::parse(&json!({"x": 1, "z": -2.5})).unwrap(), Target::Point([1.0, -2.5]));
    assert!(Target::parse(&json!({"x": 1})).is_err());
    assert!(Target::parse(&json!(3)).is_err());
}

#[test]
fn walking_goes_around_furniture_at_walking_speed() {
    let mut w = World::new();
    let len = w.move_to(&Target::Point([3.5, 1.0])).unwrap();
    assert_eq!(w.vesper.pose, Pose::Walk);
    // Every leg of the path stays clear of furniture.
    let mut p = w.vesper.pos;
    for q in w.vesper.path.clone() {
        assert!(w.clear(p, q), "{p:?} → {q:?} cuts through something");
        p = q;
    }
    let t0 = w.t;
    let events = walk(&mut w);
    assert_eq!(events, [Event::Arrived { at: None }]);
    assert!(dist(w.vesper.pos, [3.5, 1.0]) < 1e-3);
    assert!(((w.t - t0) as f32 - len / SPEED).abs() < 0.1, "took {} s for {len} m", w.t - t0);
    assert_eq!(w.vesper.pose, Pose::Stand);
}

#[test]
fn a_path_around_the_armchair_is_longer_than_the_straight_line() {
    let mut w = World::new();
    w.vesper.pos = [3.0, -2.1];
    let len = w.move_to(&Target::Point([3.0, -0.4])).unwrap();
    assert!(len > 1.9, "straight through the chair: {len}");
}

#[test]
fn bad_targets_are_refused() {
    let mut w = World::new();
    assert!(w.move_to(&Target::Point([9.0, 0.0])).unwrap_err().contains("outside"));
    assert!(w.move_to(&thing("piano")).unwrap_err().contains("look_around"));
    // A point inside furniture ends at the nearest free spot.
    w.move_to(&Target::Point([2.3, -2.8])).unwrap();
    walk(&mut w);
    assert!(w.object("bookshelf").unwrap().distance(w.vesper.pos) >= RADIUS - 1e-3);
}

#[test]
fn every_thing_can_be_reached() {
    for o in World::new().objects.iter().filter(|o| !o.actions().is_empty()) {
        let mut w = World::new();
        go(&mut w, &o.id);
        let o = w.object(&o.id).unwrap();
        assert!(w.reach_of(o) <= REACH, "{} is {} m away", o.id, w.reach_of(o));
        assert!((w.vesper.facing - face(w.vesper.pos, w.floor_pos(o))).abs() < 1e-3, "faces the {}", o.id);
    }
}

#[test]
fn things_out_of_reach_cant_be_used() {
    let mut w = World::new();
    let e = w.interact("coffee_maker", "brew", 12).unwrap_err();
    assert!(e.contains("move_to"), "{e}");
    let e = w.interact("coffee_maker", "dance", 12).unwrap_err();
    assert!(e.contains("brew, pour"), "{e}");
    assert!(w.interact("sofa", "sit", 12).is_err());
}

#[test]
fn the_coffee_timeline() {
    let mut w = World::new();
    go(&mut w, "coffee_maker");
    assert!(w.interact("coffee_maker", "pour", 12).unwrap_err().contains("brew first"));
    assert_eq!(w.interact("coffee_maker", "brew", 12).unwrap(), "brewing; ready in 20 s");
    assert_eq!(w.vesper.action.as_ref().unwrap().0, "reach");
    assert!(w.interact("coffee_maker", "brew", 12).is_err());
    let t = w.t;
    assert!(w.tick(t + 10.0).is_empty());
    assert!(w.interact("coffee_maker", "pour", 12).unwrap_err().contains("ready in 10 s"));
    let seen = w.look_around(&[]);
    let brew = &seen["things"].as_array().unwrap().iter().find(|t| t["id"] == "coffee_maker").unwrap()["state"]["brew"];
    assert_eq!(brew, &json!({"state": "brewing", "ready_in_s": 10.0}));
    assert_eq!(w.tick(t + 20.0), [Event::CoffeeReady]);
    assert!(w.tick(t + 30.0).is_empty(), "announced once");
    assert!(w.interact("coffee_maker", "pour", 12).unwrap_err().contains("pick up the mug"));
    // The mug is on the same counter, in reach from here.
    w.interact("mug", "pick_up", 12).unwrap();
    assert!(w.interact("mug", "drink", 12).unwrap_err().contains("empty"));
    assert_eq!(w.interact("coffee_maker", "pour", 12).unwrap(), "poured a mug of coffee");
    assert!(matches!(w.object("coffee_maker").unwrap().state, State::CoffeeMaker { brew: Brew::Idle }));
    assert_eq!(w.look_around(&[])["you"]["holding"], "mug");
    // She carries it.
    w.move_to(&Target::Point([0.0, 1.0])).unwrap();
    walk(&mut w);
    assert!(w.interact("mug", "drink", 12).is_ok());
    assert!(w.interact("mug", "drink", 12).is_err());
    assert_eq!(w.interact("mug", "put_down", 12).unwrap(), "put the mug down on the floor");
    assert_eq!(w.object("mug").unwrap().pos, [0.0, 0.0, 1.0]);
    assert!(w.interact("mug", "put_down", 12).is_err());
}

#[test]
fn the_mug_goes_on_a_surface_in_reach() {
    let mut w = World::new();
    go(&mut w, "mug");
    w.interact("mug", "pick_up", 12).unwrap();
    go(&mut w, "side_table");
    assert_eq!(w.interact("mug", "put_down", 12).unwrap(), "put the mug down on the side table");
    let m = w.object("mug").unwrap();
    assert_eq!(m.pos[1], 0.6);
    assert_eq!(w.object("side_table").unwrap().distance([m.pos[0], m.pos[2]]), 0.0);
}

#[test]
fn lamp_records_books_window() {
    let mut w = World::new();
    go(&mut w, "lamp");
    assert_eq!(w.interact("lamp", "toggle", 12).unwrap(), "the lamp is on");
    assert_eq!(w.interact("lamp", "toggle", 12).unwrap(), "the lamp is off");
    go(&mut w, "record_player");
    assert!(w.interact("record_player", "stop", 12).is_err());
    assert_eq!(w.interact("record_player", "play", 12).unwrap(), format!("playing \"{}\"", TRACKS[0]));
    assert_eq!(w.interact("record_player", "play", 12).unwrap(), format!("playing \"{}\"", TRACKS[1]), "next track");
    assert!(w.interact("record_player", "stop", 12).unwrap().contains(TRACKS[1]));
    go(&mut w, "bookshelf");
    assert!(w.interact("bookshelf", "browse", 12).unwrap().contains("Carmilla"));
    go(&mut w, "window");
    assert!(w.interact("window", "look_outside", 23).unwrap().starts_with("night"));
    assert!(w.interact("window", "look_outside", 12).unwrap().starts_with("grey daylight"));
}

#[test]
fn sitting_and_getting_up_by_walking_away() {
    let mut w = World::new();
    go(&mut w, "armchair");
    assert!(w.interact("armchair", "stand", 12).is_err());
    w.interact("armchair", "sit", 12).unwrap();
    assert_eq!(w.vesper.pose, Pose::Sit);
    assert_eq!(w.vesper.pos, [3.0, -1.2]);
    assert!(matches!(w.object("armchair").unwrap().state, State::Armchair { occupied: true }));
    assert!(w.interact("armchair", "sit", 12).is_err());
    // Walking off stands her up first.
    w.move_to(&thing("window")).unwrap();
    assert!(matches!(w.object("armchair").unwrap().state, State::Armchair { occupied: false }));
    walk(&mut w);
    assert!(w.reach_of(w.object("window").unwrap()) <= REACH);
    go(&mut w, "armchair");
    w.interact("armchair", "sit", 12).unwrap();
    w.interact("armchair", "stand", 12).unwrap();
    assert_eq!(w.vesper.pose, Pose::Stand);
    assert!(w.object("armchair").unwrap().distance(w.vesper.pos) > RADIUS);
}

#[test]
fn gestures_talking_and_looking() {
    let mut w = World::new();
    assert_eq!(w.gesture("wave").unwrap(), 2.0);
    assert!(w.gesture("dab").unwrap_err().contains("wave, nod, shrug, think"));
    w.tick(1.0);
    assert_eq!(w.vesper.action.as_ref().unwrap().0, "wave");
    w.tick(2.5);
    assert!(w.vesper.action.is_none());
    w.speak(3.0);
    assert_eq!(w.vesper.action, Some(("talk".into(), 5.5)));
    let people = vec!["Alice".to_string()];
    w.look_at(&thing("alice"), &people).unwrap();
    assert!(w.vesper.facing.abs() < 1e-3, "the viewers are in front");
    w.look_at(&Target::Point([-1.0, 0.0]), &people).unwrap();
    assert!((w.vesper.facing + std::f32::consts::FRAC_PI_2).abs() < 1e-3);
    assert!(w.look_at(&thing("bob"), &people).is_err());
    w.look_at(&thing("window"), &people).unwrap();
    assert!((w.vesper.facing.abs() - std::f32::consts::PI).abs() < 0.3);
}

#[test]
fn look_around_lists_usable_things() {
    let mut w = World::new();
    go(&mut w, "lamp");
    let v = w.look_around(&["alice".into()]);
    assert_eq!(v["people"], json!(["alice"]));
    let ids: Vec<&str> = v["things"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["coffee_maker", "mug", "window", "record_player", "bookshelf", "lamp", "armchair"]);
    let lamp = v["things"].as_array().unwrap().iter().find(|t| t["id"] == "lamp").unwrap();
    assert_eq!(lamp["in_reach"], true);
    assert_eq!(lamp["state"], json!({"on": false}));
    assert_eq!(lamp["actions"], json!(["toggle"]));
}
