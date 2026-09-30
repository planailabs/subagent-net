//! The room, pure: where things are, what they're doing, where Vesper
//! walks and what her actions change. No I/O and no clock; the caller
//! passes the time (seconds) in.
//!
//! Coordinates: metres, floor is x ∈ [-4, 4] (left to right), z ∈ [-3, 3]
//! (back wall to the open front where the viewers are). `facing` is the
//! angle around y, 0 = looking towards +z (three.js `rotation.y`).

use std::collections::{BinaryHeap, HashMap};

use serde::Serialize;
use serde_json::{Value, json};

pub const HALF_W: f32 = 4.0;
pub const HALF_D: f32 = 3.0;
pub const CELL: f32 = 0.25;
const COLS: usize = (2.0 * HALF_W / CELL) as usize;
const ROWS: usize = (2.0 * HALF_D / CELL) as usize;
/// Her body radius: how far she keeps from walls and furniture.
const RADIUS: f32 = 0.25;
pub const SPEED: f32 = 1.2;
/// How close she must be to use a thing.
pub const REACH: f32 = 1.2;
/// Where she walks to when sent to an object: this close to it.
const APPROACH: f32 = 0.9;
pub const BREW_SECS: f64 = 20.0;
/// Where the viewers are: she looks here when she looks at a person.
pub const VIEWERS: [f32; 2] = [0.0, 4.5];

pub const GESTURES: &[(&str, f64)] = &[("wave", 2.0), ("nod", 1.2), ("shrug", 1.5), ("think", 3.0)];
pub const TRACKS: &[&str] = &["Nocturne in Violet", "Glass Cathedral", "Rain over Lindenstraße", "Velvet Static"];
pub const BOOKS: &[&str] = &["Frankenstein", "Dracula", "Carmilla", "The Picture of Dorian Gray", "Wuthering Heights", "Poems of Emily Dickinson"];

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum State {
    CoffeeMaker { brew: Brew },
    Mug { held: bool, coffee: bool },
    Lamp { on: bool },
    RecordPlayer { track: Option<String> },
    Armchair { occupied: bool },
    Bookshelf,
    Window,
    /// Not usable; it's in the way, and maybe a surface.
    Furniture,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Brew {
    Idle,
    Brewing { ready_at: f64 },
    Ready,
}

#[derive(Debug, Clone, Serialize)]
pub struct Object {
    pub id: String,
    /// Centre of the footprint on the floor; `y` is the height it stands on.
    pub pos: [f32; 3],
    /// Half extents of the footprint (x, z); zero for small things.
    pub half: [f32; 2],
    pub rot: f32,
    /// In the way of walking.
    pub blocks: bool,
    /// Height of its top, if things can be put on it.
    pub surface: Option<f32>,
    pub state: State,
}

impl Object {
    fn new(id: &str, pos: [f32; 3], half: [f32; 2], rot: f32, state: State) -> Self {
        let blocks = !matches!(state, State::Mug { .. } | State::Window);
        Object { id: id.into(), pos, half, rot, blocks, surface: None, state }
    }

    fn surface(mut self, h: f32) -> Self {
        self.surface = Some(h);
        self
    }

    /// Distance from a floor point to the footprint.
    pub fn distance(&self, p: [f32; 2]) -> f32 {
        let dx = ((p[0] - self.pos[0]).abs() - self.half[0]).max(0.0);
        let dz = ((p[1] - self.pos[2]).abs() - self.half[1]).max(0.0);
        dx.hypot(dz)
    }

    pub fn actions(&self) -> &'static [&'static str] {
        match self.state {
            State::CoffeeMaker { .. } => &["brew", "pour"],
            State::Mug { .. } => &["pick_up", "drink", "put_down"],
            State::Lamp { .. } => &["toggle"],
            State::RecordPlayer { .. } => &["play", "stop"],
            State::Armchair { .. } => &["sit", "stand"],
            State::Bookshelf => &["browse"],
            State::Window => &["look_outside"],
            State::Furniture => &[],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pose {
    Stand,
    Walk,
    Sit,
}

#[derive(Debug, Clone, Serialize)]
pub struct Vesper {
    pub pos: [f32; 2],
    pub facing: f32,
    pub pose: Pose,
    /// A clip playing on top of the pose (`talk`, `wave`, `reach`, …) and
    /// when it ends.
    pub action: Option<(String, f64)>,
    /// Remaining waypoints while walking.
    pub path: Vec<[f32; 2]>,
    #[serde(skip)]
    walk_to: Option<String>,
}

/// Where to go or look.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Thing(String),
    Point([f32; 2]),
}

impl Target {
    /// `"coffee_maker"`, a person's name, or `{"x": 1, "z": -2}`.
    pub fn parse(v: &Value) -> Result<Target, String> {
        match v {
            Value::String(s) => Ok(Target::Thing(s.trim().to_string())),
            Value::Object(o) => match (o.get("x").and_then(Value::as_f64), o.get("z").and_then(Value::as_f64)) {
                (Some(x), Some(z)) => Ok(Target::Point([x as f32, z as f32])),
                _ => Err("a point needs numeric x and z".into()),
            },
            _ => Err("target is an object id, a person's name or {x, z}".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// She reached where she was walking to.
    Arrived { at: Option<String> },
    CoffeeReady,
}

#[derive(Debug, Clone)]
pub struct World {
    pub t: f64,
    pub vesper: Vesper,
    pub objects: Vec<Object>,
    free: Vec<bool>,
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl World {
    pub fn new() -> Self {
        use State::*;
        let objects = vec![
            Object::new("counter", [-2.9, 0.0, -2.6], [1.0, 0.35], 0.0, Furniture).surface(0.9),
            Object::new("coffee_maker", [-3.4, 0.9, -2.65], [0.0, 0.0], 0.0, CoffeeMaker { brew: Brew::Idle }),
            Object::new("mug", [-2.7, 0.9, -2.6], [0.0, 0.0], 0.0, Mug { held: false, coffee: false }),
            Object::new("window", [-0.6, 1.0, -3.0], [0.7, 0.0], 0.0, Window),
            Object::new("record_player", [0.9, 0.0, -2.7], [0.45, 0.25], 0.0, RecordPlayer { track: None }).surface(0.55),
            Object::new("bookshelf", [2.3, 0.0, -2.8], [0.6, 0.18], 0.0, Bookshelf),
            Object::new("lamp", [3.6, 0.0, -2.5], [0.2, 0.2], 0.0, Lamp { on: false }),
            Object::new("armchair", [3.0, 0.0, -1.2], [0.45, 0.45], -std::f32::consts::FRAC_PI_2, Armchair { occupied: false }),
            Object::new("side_table", [3.1, 0.0, 0.0], [0.3, 0.3], 0.0, Furniture).surface(0.6),
        ];
        let mut w = World {
            t: 0.0,
            vesper: Vesper { pos: [0.0, 0.0], facing: 0.0, pose: Pose::Stand, action: None, path: vec![], walk_to: None },
            objects,
            free: vec![],
        };
        w.free = (0..COLS * ROWS).map(|i| w.cell_free(i % COLS, i / COLS)).collect();
        w
    }

    fn cell_free(&self, c: usize, r: usize) -> bool {
        let [x, z] = center(c, r);
        if x.abs() > HALF_W - RADIUS || z.abs() > HALF_D - RADIUS {
            return false;
        }
        !self.objects.iter().any(|o| o.blocks && o.distance([x, z]) < RADIUS)
    }

    pub fn object(&self, id: &str) -> Option<&Object> {
        self.objects.iter().find(|o| o.id == id)
    }

    fn object_mut(&mut self, id: &str) -> Option<&mut Object> {
        self.objects.iter_mut().find(|o| o.id == id)
    }

    /// Where a thing is on the floor, for distances. A held mug is in her hand.
    fn floor_pos(&self, o: &Object) -> [f32; 2] {
        match o.state {
            State::Mug { held: true, .. } => self.vesper.pos,
            _ => [o.pos[0], o.pos[2]],
        }
    }

    fn reach_of(&self, o: &Object) -> f32 {
        let p = self.floor_pos(o);
        let o = Object { pos: [p[0], 0.0, p[1]], ..o.clone() };
        o.distance(self.vesper.pos)
    }

    pub fn walking(&self) -> bool {
        !self.vesper.path.is_empty()
    }

    /// Advances time: walking, finished clips, the coffee.
    pub fn tick(&mut self, now: f64) -> Vec<Event> {
        let dt = (now - self.t).max(0.0);
        self.t = now;
        let mut events = vec![];
        if let Some((_, until)) = &self.vesper.action
            && *until <= now
        {
            self.vesper.action = None;
        }
        for o in &mut self.objects {
            if let State::CoffeeMaker { brew: b @ Brew::Brewing { .. } } = &mut o.state
                && let Brew::Brewing { ready_at } = *b
                && ready_at <= now
            {
                *b = Brew::Ready;
                events.push(Event::CoffeeReady);
            }
        }
        if self.walking() {
            let mut budget = SPEED * dt as f32;
            while budget > 0.0 && !self.vesper.path.is_empty() {
                let [px, pz] = self.vesper.pos;
                let [tx, tz] = self.vesper.path[0];
                let d = (tx - px).hypot(tz - pz);
                if d > 1e-4 {
                    self.vesper.facing = (tx - px).atan2(tz - pz);
                }
                if d <= budget {
                    self.vesper.pos = [tx, tz];
                    self.vesper.path.remove(0);
                    budget -= d;
                } else {
                    let f = budget / d;
                    self.vesper.pos = [px + (tx - px) * f, pz + (tz - pz) * f];
                    budget = 0.0;
                }
            }
            if self.vesper.path.is_empty() {
                self.vesper.pose = Pose::Stand;
                let at = self.vesper.walk_to.take();
                if let Some(o) = at.as_deref().and_then(|id| self.object(id)) {
                    self.vesper.facing = face(self.vesper.pos, self.floor_pos(o));
                }
                events.push(Event::Arrived { at });
            }
        }
        events
    }

    /// Starts walking. Returns the path length in metres (0 if already there).
    pub fn move_to(&mut self, target: &Target) -> Result<f32, String> {
        self.stand_up();
        let start = self.nearest_free(self.vesper.pos).ok_or("she's stuck")?;
        let (cells, end, at) = match target {
            Target::Point(p) => {
                if p[0].abs() > HALF_W || p[1].abs() > HALF_D {
                    return Err(format!("({}, {}) is outside the room", p[0], p[1]));
                }
                let goal = self.nearest_free(*p).ok_or("no free floor there")?;
                let cells = self.search(start, |c| c == goal).ok_or("no way there")?;
                let end = if self.free[idx(cell_of(*p))] { *p } else { center(goal.0, goal.1) };
                (cells, end, None)
            }
            Target::Thing(id) => {
                let o = self.object(id).ok_or_else(|| format!("there's no {id:?} here; see look_around"))?;
                let o = Object { pos: { let p = self.floor_pos(o); [p[0], 0.0, p[1]] }, ..o.clone() };
                let cells = self.search(start, |(c, r)| o.distance(center(c, r)) <= APPROACH).ok_or_else(|| format!("can't get to the {id}"))?;
                let last = *cells.last().unwrap();
                (cells, center(last.0, last.1), Some(id.clone()))
            }
        };
        let mut points: Vec<[f32; 2]> = cells.iter().map(|&(c, r)| center(c, r)).collect();
        *points.last_mut().unwrap() = end;
        let path = self.smooth(self.vesper.pos, &points);
        let mut len = 0.0;
        let mut p = self.vesper.pos;
        for q in &path {
            len += (q[0] - p[0]).hypot(q[1] - p[1]);
            p = *q;
        }
        if len < 1e-3 {
            self.vesper.path.clear();
            if let Some(o) = at.as_deref().and_then(|id| self.object(id)) {
                self.vesper.facing = face(self.vesper.pos, self.floor_pos(o));
            }
            return Ok(0.0);
        }
        self.vesper.path = path;
        self.vesper.pose = Pose::Walk;
        self.vesper.walk_to = at;
        Ok(len)
    }

    /// Turns towards a thing, a person (the viewers) or a point.
    pub fn look_at(&mut self, target: &Target, people: &[String]) -> Result<(), String> {
        let p = match target {
            Target::Point(p) => *p,
            Target::Thing(id) => match self.object(id) {
                Some(o) => self.floor_pos(o),
                None if people.iter().any(|p| p.eq_ignore_ascii_case(id)) => VIEWERS,
                None => return Err(format!("{id:?} is neither a thing here nor someone in the room")),
            },
        };
        self.vesper.facing = face(self.vesper.pos, p);
        Ok(())
    }

    /// Plays a gesture clip; returns its length.
    pub fn gesture(&mut self, name: &str) -> Result<f64, String> {
        let (name, secs) = GESTURES.iter().find(|(g, _)| *g == name).ok_or_else(|| {
            format!("unknown gesture {name:?}; one of {}", GESTURES.iter().map(|g| g.0).collect::<Vec<_>>().join(", "))
        })?;
        self.vesper.action = Some((name.to_string(), self.t + secs));
        Ok(*secs)
    }

    /// She talks for `secs` (her mouth follows the audio in browsers).
    pub fn speak(&mut self, secs: f64) {
        self.vesper.action = Some(("talk".into(), self.t + secs));
    }

    fn stand_up(&mut self) {
        if self.vesper.pose == Pose::Sit {
            self.vesper.pose = Pose::Stand;
            if let Some(o) = self.object_mut("armchair") {
                o.state = State::Armchair { occupied: false };
                let [x, _, z] = o.pos;
                let (s, c) = o.rot.sin_cos();
                // Step out in front of the chair.
                self.vesper.pos = [x + s * 0.8, z + c * 0.8];
            }
        }
    }

    /// Uses a thing. `hour` is the local hour of day (for the window).
    pub fn interact(&mut self, id: &str, action: &str, hour: u32) -> Result<String, String> {
        let o = self.object(id).ok_or_else(|| format!("there's no {id:?} here; see look_around"))?;
        if !o.actions().contains(&action) {
            return Err(format!("the {id} can't {action}; it can: {}", o.actions().join(", ")));
        }
        let d = self.reach_of(o);
        if d > REACH {
            return Err(format!("the {id} is {d:.1} m away; move_to it first"));
        }
        let now = self.t;
        let mug = |w: &World| match w.object("mug").map(|m| &m.state) {
            Some(State::Mug { held, coffee }) => (*held, *coffee),
            _ => (false, false),
        };
        let reply = match (id, action) {
            ("coffee_maker", "brew") => match self.brew() {
                Brew::Idle => {
                    self.set_brew(Brew::Brewing { ready_at: now + BREW_SECS });
                    format!("brewing; ready in {BREW_SECS:.0} s")
                }
                Brew::Brewing { ready_at } => return Err(format!("already brewing; ready in {:.0} s", ready_at - now)),
                Brew::Ready => return Err("there's fresh coffee already; pour it".into()),
            },
            ("coffee_maker", "pour") => {
                let (held, coffee) = mug(self);
                match self.brew() {
                    Brew::Ready if held && !coffee => {
                        self.set_brew(Brew::Idle);
                        self.set_mug(true, true, None);
                        "poured a mug of coffee".into()
                    }
                    Brew::Ready if !held => return Err("pick up the mug first".into()),
                    Brew::Ready => return Err("the mug is full already".into()),
                    Brew::Brewing { ready_at } => return Err(format!("still brewing; ready in {:.0} s", ready_at - now)),
                    Brew::Idle => return Err("there's no coffee; brew first".into()),
                }
            }
            ("mug", "pick_up") => match mug(self) {
                (true, _) => return Err("already holding it".into()),
                (false, coffee) => {
                    self.set_mug(true, coffee, None);
                    format!("holding the mug ({})", if coffee { "coffee" } else { "empty" })
                }
            },
            ("mug", "drink") => match mug(self) {
                (true, true) => {
                    self.set_mug(true, false, None);
                    "drank the coffee; the mug is empty".into()
                }
                (true, false) => return Err("the mug is empty".into()),
                (false, _) => return Err("pick up the mug first".into()),
            },
            ("mug", "put_down") => match mug(self) {
                (true, coffee) => {
                    let spot = self.put_down_spot();
                    self.set_mug(false, coffee, Some(spot.0));
                    format!("put the mug down {}", spot.1)
                }
                (false, _) => return Err("not holding the mug".into()),
            },
            ("lamp", "toggle") => {
                let o = self.object_mut(id).unwrap();
                let State::Lamp { on } = &mut o.state else { unreachable!() };
                *on = !*on;
                format!("the lamp is {}", if *on { "on" } else { "off" })
            }
            ("record_player", "play") => {
                let o = self.object_mut(id).unwrap();
                let State::RecordPlayer { track } = &mut o.state else { unreachable!() };
                let next = match track.as_deref().and_then(|t| TRACKS.iter().position(|x| *x == t)) {
                    Some(i) => TRACKS[(i + 1) % TRACKS.len()],
                    None => TRACKS[0],
                };
                *track = Some(next.to_string());
                format!("playing \"{next}\"")
            }
            ("record_player", "stop") => {
                let o = self.object_mut(id).unwrap();
                let State::RecordPlayer { track } = &mut o.state else { unreachable!() };
                match track.take() {
                    Some(t) => format!("stopped \"{t}\""),
                    None => return Err("nothing is playing".into()),
                }
            }
            ("armchair", "sit") => {
                if self.vesper.pose == Pose::Sit {
                    return Err("already sitting".into());
                }
                let o = self.object_mut(id).unwrap();
                o.state = State::Armchair { occupied: true };
                let (pos, rot) = ([o.pos[0], o.pos[2]], o.rot);
                self.vesper.path.clear();
                self.vesper.walk_to = None;
                self.vesper.pos = pos;
                self.vesper.facing = rot;
                self.vesper.pose = Pose::Sit;
                "sitting in the armchair".into()
            }
            ("armchair", "stand") => {
                if self.vesper.pose != Pose::Sit {
                    return Err("not sitting".into());
                }
                self.stand_up();
                "standing up".into()
            }
            ("bookshelf", "browse") => format!("on the shelf: {}", BOOKS.join(", ")),
            ("window", "look_outside") => outside(hour).into(),
            _ => unreachable!("actions() and interact() disagree on {id}.{action}"),
        };
        if !matches!(action, "sit" | "stand" | "look_outside") {
            self.vesper.action = Some(("reach".into(), now + 1.0));
        }
        if id != "mug" || action == "pick_up" {
            self.vesper.facing = face(self.vesper.pos, self.floor_pos(self.object(id).unwrap()));
        }
        Ok(reply)
    }

    fn brew(&self) -> Brew {
        match self.object("coffee_maker").map(|o| &o.state) {
            Some(State::CoffeeMaker { brew }) => *brew,
            _ => Brew::Idle,
        }
    }

    fn set_brew(&mut self, b: Brew) {
        self.object_mut("coffee_maker").unwrap().state = State::CoffeeMaker { brew: b };
    }

    fn set_mug(&mut self, held: bool, coffee: bool, at: Option<[f32; 3]>) {
        let m = self.object_mut("mug").unwrap();
        m.state = State::Mug { held, coffee };
        if let Some(at) = at {
            m.pos = at;
        }
    }

    /// The nearest surface in reach, else the floor at her feet.
    fn put_down_spot(&self) -> ([f32; 3], String) {
        let p = self.vesper.pos;
        let best = self
            .objects
            .iter()
            .filter_map(|o| o.surface.map(|h| (o, h, o.distance(p))))
            .filter(|(_, _, d)| *d <= REACH)
            .min_by(|a, b| a.2.total_cmp(&b.2));
        match best {
            Some((o, h, _)) => {
                let x = p[0].clamp(o.pos[0] - o.half[0] + 0.1, o.pos[0] + o.half[0] - 0.1);
                let z = p[1].clamp(o.pos[2] - o.half[1] + 0.1, o.pos[2] + o.half[1] - 0.1);
                ([x, h, z], format!("on the {}", o.id.replace('_', " ")))
            }
            None => ([p[0], 0.0, p[1]], "on the floor".into()),
        }
    }

    /// What she sees: herself, the people, every thing with its state,
    /// distance and actions.
    pub fn look_around(&self, people: &[String]) -> Value {
        let things: Vec<Value> = self
            .objects
            .iter()
            .filter(|o| !o.actions().is_empty())
            .map(|o| {
                let mut state = serde_json::to_value(&o.state).unwrap();
                state.as_object_mut().unwrap().remove("kind");
                if let Some(Value::Object(b)) = state.get_mut("brew")
                    && let Some(r) = b.remove("ready_at")
                {
                    b.insert("ready_in_s".into(), json!((r.as_f64().unwrap() - self.t).max(0.0).round()));
                }
                json!({
                    "id": o.id,
                    "distance_m": (self.reach_of(o) * 10.0).round() / 10.0,
                    "in_reach": self.reach_of(o) <= REACH,
                    "actions": o.actions(),
                    "state": state,
                })
            })
            .collect();
        json!({
            "you": {
                "x": round2(self.vesper.pos[0]), "z": round2(self.vesper.pos[1]),
                "pose": self.vesper.pose,
                "holding": matches!(self.object("mug").map(|m| &m.state), Some(State::Mug { held: true, .. })).then_some("mug"),
            },
            "room": "8 × 6 m: kitchen corner on the left, window at the back, reading corner on the right; the people watch from the open front",
            "people": people,
            "things": things,
        })
    }

    /// The nearest walkable cell to a point.
    fn nearest_free(&self, p: [f32; 2]) -> Option<(usize, usize)> {
        (0..COLS * ROWS)
            .filter(|&i| self.free[i])
            .map(|i| (i % COLS, i / COLS))
            .min_by(|a, b| dist(center(a.0, a.1), p).total_cmp(&dist(center(b.0, b.1), p)))
    }

    /// Dijkstra over the grid (8 neighbours, no corner cutting) to the
    /// nearest cell satisfying `goal`.
    fn search(&self, start: (usize, usize), goal: impl Fn((usize, usize)) -> bool) -> Option<Vec<(usize, usize)>> {
        #[derive(PartialEq)]
        struct Q(f32, usize);
        impl Eq for Q {}
        impl Ord for Q {
            fn cmp(&self, o: &Self) -> std::cmp::Ordering {
                o.0.total_cmp(&self.0)
            }
        }
        impl PartialOrd for Q {
            fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(o))
            }
        }
        let mut cost = vec![f32::INFINITY; COLS * ROWS];
        let mut prev: HashMap<usize, usize> = HashMap::new();
        let mut heap = BinaryHeap::new();
        cost[idx(start)] = 0.0;
        heap.push(Q(0.0, idx(start)));
        while let Some(Q(c, i)) = heap.pop() {
            if c > cost[i] {
                continue;
            }
            let (x, y) = (i % COLS, i / COLS);
            if goal((x, y)) {
                let mut path = vec![(x, y)];
                let mut i = i;
                while let Some(&p) = prev.get(&i) {
                    path.push((p % COLS, p / COLS));
                    i = p;
                }
                path.reverse();
                return Some(path);
            }
            for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (-1, 1), (1, -1), (1, 1)] {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if nx < 0 || ny < 0 || nx >= COLS as i32 || ny >= ROWS as i32 {
                    continue;
                }
                let (nx, ny) = (nx as usize, ny as usize);
                let ok = |c: usize, r: usize| self.free[r * COLS + c];
                if !ok(nx, ny) || (dx != 0 && dy != 0 && (!ok(nx, y) || !ok(x, ny))) {
                    continue;
                }
                let n = ny * COLS + nx;
                let nc = c + if dx != 0 && dy != 0 { std::f32::consts::SQRT_2 } else { 1.0 };
                if nc < cost[n] {
                    cost[n] = nc;
                    prev.insert(n, i);
                    heap.push(Q(nc, n));
                }
            }
        }
        None
    }

    /// Drops waypoints she can see past (string pulling).
    fn smooth(&self, from: [f32; 2], points: &[[f32; 2]]) -> Vec<[f32; 2]> {
        let mut out = vec![];
        let mut cur = from;
        let mut i = 0;
        while i < points.len() {
            let mut j = points.len() - 1;
            while j > i && !self.clear(cur, points[j]) {
                j -= 1;
            }
            out.push(points[j]);
            cur = points[j];
            i = j + 1;
        }
        out
    }

    /// Whether a straight walk stays on free cells.
    fn clear(&self, a: [f32; 2], b: [f32; 2]) -> bool {
        let n = (dist(a, b) / 0.05).ceil().max(1.0) as usize;
        (0..=n).all(|k| {
            let f = k as f32 / n as f32;
            let p = [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f];
            let c = cell_of(p);
            // Her start may be off-grid (just stood up); only judge cells she enters.
            self.free[idx(c)] || cell_of(a) == c
        })
    }
}

fn center(c: usize, r: usize) -> [f32; 2] {
    [-HALF_W + (c as f32 + 0.5) * CELL, -HALF_D + (r as f32 + 0.5) * CELL]
}

fn cell_of(p: [f32; 2]) -> (usize, usize) {
    let c = ((p[0] + HALF_W) / CELL).floor().clamp(0.0, COLS as f32 - 1.0) as usize;
    let r = ((p[1] + HALF_D) / CELL).floor().clamp(0.0, ROWS as f32 - 1.0) as usize;
    (c, r)
}

fn idx((c, r): (usize, usize)) -> usize {
    r * COLS + c
}

fn dist(a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

fn face(from: [f32; 2], to: [f32; 2]) -> f32 {
    (to[0] - from[0]).atan2(to[1] - from[1])
}

fn round2(x: f32) -> f32 {
    (x * 100.0).round() / 100.0
}

fn outside(hour: u32) -> &'static str {
    match hour {
        5..=7 => "dawn: a pale strip of light over wet rooftops",
        8..=16 => "grey daylight, a quiet street, pigeons on the gutter",
        17..=20 => "dusk: violet sky, streetlights flickering on",
        _ => "night: rain on the glass, the street lamps humming orange",
    }
}

#[cfg(test)]
mod tests;
