// The 3D room: loads the Blender exports, follows the server's state,
// animates Vesper and moves her mouth with her voice.
import * as THREE from "three";
import { GLTFLoader } from "three/examples/jsm/loaders/GLTFLoader.js";
import { OrbitControls } from "three/examples/jsm/controls/OrbitControls.js";
import { brewProgress, clipWeights, isLegTrack, mouthAt, skyColor, smooth, smoothAngle, stepWeights } from "./logic.js";

const load = (url) => new Promise((ok, err) => new GLTFLoader().load(url, ok, undefined, err));

export class RoomScene {
  constructor(canvas) {
    this.renderer = new THREE.WebGLRenderer({ canvas, antialias: true });
    this.renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
    this.renderer.outputColorSpace = THREE.SRGBColorSpace;
    this.renderer.toneMapping = THREE.ACESFilmicToneMapping;
    this.renderer.toneMappingExposure = 1.5;
    this.scene = new THREE.Scene();
    this.scene.background = new THREE.Color(0x050407);
    this.camera = new THREE.PerspectiveCamera(45, 1, 0.05, 50);
    this.camera.position.set(1.0, 1.8, 5.4);
    this.controls = new OrbitControls(this.camera, canvas);
    this.controls.target.set(0.7, 1.0, -1.0);
    Object.assign(this.controls, { enableDamping: true, minDistance: 1.2, maxDistance: 7, maxPolarAngle: Math.PI * 0.52, enablePan: false });

    this.scene.add(new THREE.HemisphereLight(0x9a8fb4, 0x2a2020, 1.4));
    // A soft fill from the viewers' side, so her face isn't in shadow.
    const fill = new THREE.DirectionalLight(0xffe2d0, 1.1);
    fill.position.set(1.5, 2.5, 5);
    this.scene.add(fill);
    this.window = new THREE.DirectionalLight(0x9aa6c8, 0.9);
    this.window.position.set(-0.6, 2.2, -4);
    this.scene.add(this.window);
    this.lampLight = new THREE.PointLight(0xffc98a, 0, 6, 1.5);
    this.candle = new THREE.PointLight(0xff9a40, 0.6, 2.5, 2);
    this.scene.add(this.lampLight, this.candle);

    this.state = null;
    this.weights = {};
    this.actions = {};
    this.speech = null;
    this.timer = new THREE.Timer();
    this.resize();
    addEventListener("resize", () => this.resize());
  }

  resize() {
    const c = this.renderer.domElement;
    const w = c.clientWidth || innerWidth, h = c.clientHeight || innerHeight;
    this.renderer.setSize(w, h, false);
    this.camera.aspect = w / h;
    this.camera.updateProjectionMatrix();
  }

  async load(base = "/assets/") {
    const [room, props, vesper] = await Promise.all(["room.glb", "props.glb", "vesper.glb"].map((f) => load(base + f)));
    this.scene.add(room.scene, props.scene, vesper.scene);
    this.room = room.scene;
    this.props = props.scene;
    this.vesper = vesper.scene;
    this.sky = room.scene.getObjectByName("sky");
    this.hand = vesper.scene.getObjectByName("hand_R");
    this.mouths = [];
    vesper.scene.traverse((o) => {
      if (o.morphTargetDictionary && "mouth_open" in o.morphTargetDictionary) this.mouths.push([o, o.morphTargetDictionary.mouth_open]);
      if (o.isMesh) o.frustumCulled = false; // skinned bounds don't follow the animation
    });
    // Every clip three ways: whole, legs only, upper body only.
    this.mixer = new THREE.AnimationMixer(vesper.scene);
    for (const clip of vesper.animations) {
      const variants = {
        [clip.name]: clip,
        [`${clip.name}:legs`]: new THREE.AnimationClip(`${clip.name}:legs`, clip.duration, clip.tracks.filter((t) => isLegTrack(t.name))),
        [`${clip.name}:upper`]: new THREE.AnimationClip(`${clip.name}:upper`, clip.duration, clip.tracks.filter((t) => !isLegTrack(t.name))),
      };
      for (const [name, c] of Object.entries(variants)) {
        const a = this.mixer.clipAction(c);
        a.setEffectiveWeight(0);
        a.play();
        this.actions[name] = a;
      }
    }
    this.renderer.setAnimationLoop(() => this.frame());
  }

  setState(s) {
    const prevAction = this.state?.vesper.action?.[0];
    this.state = s;
    const action = s.vesper.action?.[0];
    // A gesture starts from its first frame.
    if (action && action !== prevAction) this.actions[`${action}:upper`]?.reset();
    if (!this.placed && this.vesper) {
      this.vesper.position.set(s.vesper.pos[0], 0, s.vesper.pos[1]);
      this.vesper.rotation.y = s.vesper.facing;
      this.placed = true;
    }
  }

  /** Plays one of her speeches; the mouth follows its envelope. */
  speak(msg) {
    const audio = new Audio(msg.url);
    const started = performance.now();
    this.speech = { ...msg, audio, started };
    audio.play().catch(() => {}); // blocked autoplay: the mouth still moves by the clock
  }

  mouth() {
    const s = this.speech;
    if (!s) return 0;
    const t = !s.audio.paused && s.audio.currentTime > 0 ? s.audio.currentTime : (performance.now() - s.started) / 1000;
    if (t > s.secs + 0.2) this.speech = null;
    return Math.min(1, mouthAt(s.envelope, s.frame, t) * 1.3);
  }

  frame() {
    this.timer.update();
    const dt = Math.min(0.1, this.timer.getDelta());
    const s = this.state;
    if (s && this.vesper) {
      const v = this.vesper;
      v.position.x = smooth(v.position.x, s.vesper.pos[0], dt, 8);
      v.position.z = smooth(v.position.z, s.vesper.pos[1], dt, 8);
      v.rotation.y = smoothAngle(v.rotation.y, s.vesper.facing, dt, 8);
      this.weights = stepWeights(this.weights, clipWeights(s.vesper), dt);
      for (const [name, a] of Object.entries(this.actions)) a.setEffectiveWeight(this.weights[name] || 0);
      this.mixer.update(dt);
      const open = this.mouth();
      for (const [mesh, i] of this.mouths) mesh.morphTargetInfluences[i] = smooth(mesh.morphTargetInfluences[i], open, dt, 25);
      this.things(s, dt);
    }
    this.controls.update();
    this.renderer.render(this.scene, this.camera);
  }

  things(s, dt) {
    for (const o of s.objects) {
      const node = this.props.getObjectByName(o.id);
      if (!node) continue;
      const st = o.state;
      if (st.kind === "mug" && st.held && this.hand) {
        this.vesper.updateMatrixWorld(true);
        this.hand.getWorldPosition(node.position);
        node.position.y -= 0.09;
      } else {
        node.position.set(o.pos[0], o.pos[1], o.pos[2]);
        node.rotation.y = o.rot;
      }
      const part = (name) => node.getObjectByName(name);
      if (st.kind === "coffee_maker") {
        const k = brewProgress(st.brew, s.t);
        const coffee = part("coffee");
        coffee.visible = k > 0.02;
        coffee.scale.y = Math.max(0.02, k);
        part("led").visible = st.brew.state !== "idle";
      } else if (st.kind === "mug") {
        part("mug_fill").visible = st.coffee;
      } else if (st.kind === "lamp") {
        const shade = part("shade");
        shade.material.emissive?.setRGB(1, 0.8, 0.5);
        shade.material.emissiveIntensity = st.on ? 1.2 : 0;
        shade.getWorldPosition(this.lampLight.position);
        this.lampLight.intensity = st.on ? 2.5 : 0;
      } else if (st.kind === "record_player") {
        if (st.track) part("platter").rotation.y -= dt * (33.3 / 60) * 2 * Math.PI;
      } else if (o.id === "side_table") {
        const flame = part("flame");
        flame.getWorldPosition(this.candle.position);
        this.candle.intensity = 0.5 + 0.15 * Math.sin(performance.now() / 90) * Math.sin(performance.now() / 37);
      }
    }
    if (this.sky) {
      const [r, g, b] = skyColor(new Date().getHours());
      this.sky.material.color.setRGB(r, g, b);
      this.sky.material.emissive?.setRGB(r * 0.6, g * 0.6, b * 0.6);
    }
  }
}
