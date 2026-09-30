<script setup>
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from "vue";
import { RoomScene } from "./scene.js";
import { Mic } from "./mic.js";

const props = defineProps({ me: String });
const emit = defineEmits(["logout", "expired"]);
const canvas = ref(null);
const log = ref(null);
const lines = ref([]);
const people = ref([]);
const track = ref(null);
const text = ref("");
const talking = ref(false);
const status = ref("connecting");
const loading = ref(true);
let scene, ws, retry, closed = false;

const mic = new Mic((pcm) => ws?.readyState === 1 && ws.send(pcm.buffer));

function send(msg) {
  if (ws?.readyState === 1) ws.send(JSON.stringify(msg));
}

async function add(line) {
  lines.value.push(line);
  if (lines.value.length > 200) lines.value.shift();
  await nextTick();
  log.value?.scrollTo(0, log.value.scrollHeight);
}

function connect() {
  ws = new WebSocket(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws`);
  ws.binaryType = "arraybuffer";
  ws.onopen = () => (status.value = "");
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    if (m.type === "hello") {
      lines.value = [];
      m.chat.forEach(add);
    } else if (m.type === "state") {
      scene?.setState(m);
      people.value = m.people;
      track.value = m.objects.find((o) => o.id === "record_player")?.state.track || null;
    } else if (m.type === "chat") add(m);
    else if (m.type === "speech") scene?.speak(m);
  };
  ws.onclose = async () => {
    if (closed) return;
    status.value = "reconnecting";
    const r = await fetch("/api/me");
    if (r.status === 401) return emit("expired");
    retry = setTimeout(connect, 1500);
  };
}

function chat() {
  if (!text.value.trim()) return;
  send({ type: "chat", text: text.value });
  text.value = "";
}

async function talk(on) {
  if (on === talking.value) return;
  talking.value = on;
  if (on) {
    try {
      send({ type: "voice_start" });
      await mic.start();
    } catch (e) {
      talking.value = false;
      send({ type: "voice_cancel" });
      add({ from: null, text: `microphone: ${e.message}` });
    }
  } else {
    mic.stop();
    send({ type: "voice_end" });
  }
}

const key = (down) => (e) => {
  if (e.code === "Space" && e.target.tagName !== "INPUT" && !e.repeat) {
    e.preventDefault();
    talk(down);
  }
};
const keydown = key(true), keyup = key(false);

onMounted(async () => {
  scene = new RoomScene(canvas.value);
  connect();
  addEventListener("keydown", keydown);
  addEventListener("keyup", keyup);
  try {
    await scene.load();
  } catch (e) {
    add({ from: null, text: `couldn't load the room: ${e.message}` });
  }
  loading.value = false;
});

onBeforeUnmount(() => {
  closed = true;
  clearTimeout(retry);
  ws?.close();
  removeEventListener("keydown", keydown);
  removeEventListener("keyup", keyup);
});

const others = computed(() => people.value.filter((p) => p !== props.me));
</script>

<template>
  <div class="room">
    <canvas ref="canvas"></canvas>
    <div v-if="loading" class="veil">lighting the candles…</div>
    <aside>
      <header>
        <span class="brand">Vesper</span>
        <span class="dim grow">{{ others.length ? `with ${others.join(", ")}` : "just you" }}</span>
        <button @click="emit('logout')">leave</button>
      </header>
      <p v-if="track" class="dim np">♪ {{ track }}</p>
      <p v-if="status" class="dim np">{{ status }}…</p>
      <div ref="log" class="log">
        <p v-for="(l, i) in lines" :key="i" :class="{ her: l.from === 'vesper', sys: !l.from }">
          <b v-if="l.from">{{ l.from }}</b>
          <span :class="{ dim: l.voice }">{{ l.text }}</span>
          <span v-if="l.to" class="dim"> → {{ l.to }}</span>
        </p>
      </div>
      <form class="say" @submit.prevent="chat">
        <input v-model="text" placeholder="say something…" maxlength="2000" />
        <button
          type="button"
          :class="{ on: talking }"
          title="hold to talk (or hold space)"
          @pointerdown.prevent="talk(true)"
          @pointerup="talk(false)"
          @pointerleave="talk(false)"
        >{{ talking ? "● listening" : "hold to talk" }}</button>
      </form>
    </aside>
  </div>
</template>
