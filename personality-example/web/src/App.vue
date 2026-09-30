<script setup>
import { onMounted, ref } from "vue";
import Login from "./Login.vue";
import Room from "./Room.vue";

const me = ref(null);
const checked = ref(false);

onMounted(async () => {
  const r = await fetch("/api/me");
  if (r.ok) me.value = (await r.json()).name;
  checked.value = true;
});

async function logout() {
  await fetch("/api/logout", { method: "POST" });
  me.value = null;
}
</script>

<template>
  <Room v-if="me" :me="me" @logout="logout" @expired="me = null" />
  <Login v-else-if="checked" @in="(n) => (me = n)" />
</template>
