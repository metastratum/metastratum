<script setup lang="ts">
import { watch } from 'vue'
import { useRouter } from 'vue-router'
import TaskTray from './components/TaskTray.vue'
import { useSession } from './stores/session'
import { useTasks } from './stores/tasks'

const session = useSession()
const tasks = useTasks()
const router = useRouter()

watch(
  () => session.me,
  (me) => (me ? tasks.start() : tasks.stop()),
  { immediate: true },
)

async function signOut() {
  await session.logout()
  await router.push({ name: 'login' })
}
</script>

<template>
  <template v-if="session.me">
    <header class="bar">
      <RouterLink to="/vms" class="mark" aria-label="hypermon home">
        <span class="tape">hypermon</span>
      </RouterLink>
      <nav aria-label="Main">
        <RouterLink to="/vms">Machines</RouterLink>
        <RouterLink to="/images">Images</RouterLink>
        <RouterLink v-if="session.me.provider_admin" to="/hosts">Hosts</RouterLink>
      </nav>
      <div class="right">
        <span class="tenant" :title="`Signed in to ${session.me.tenant_name}`">
          <span class="label">Tenant</span>
          {{ session.me.tenant_name }}
        </span>
        <TaskTray />
        <button type="button" class="signout" :title="`Signed in as ${session.me.username}`" @click="signOut">Sign out</button>
      </div>
    </header>
    <main>
      <RouterView />
    </main>
  </template>
  <RouterView v-else />
</template>

<style scoped>
.bar {
  position: sticky;
  top: 0;
  z-index: 10;
  display: flex;
  align-items: center;
  gap: 28px;
  height: 52px;
  padding: 0 20px;
  background: var(--bar);
  color: var(--bar-ink);
}
.mark {
  text-decoration: none;
}
.tape {
  display: inline-block;
  padding: 3px 9px 2px;
  background: var(--tape);
  color: var(--tape-ink);
  font: 600 14px var(--font-label);
  letter-spacing: 0.14em;
  text-transform: uppercase;
  /* Cut ends, like a strip from a label printer. */
  clip-path: polygon(4px 0, 100% 0, calc(100% - 4px) 100%, 0 100%);
}
nav {
  display: flex;
  gap: 4px;
  height: 100%;
}
nav a {
  display: flex;
  align-items: center;
  padding: 0 12px;
  color: rgb(227 232 236 / 68%);
  font: 500 12px var(--font-label);
  letter-spacing: 0.12em;
  text-transform: uppercase;
  text-decoration: none;
  border-bottom: 2px solid transparent;
}
nav a:hover {
  color: var(--bar-ink);
}
nav a.router-link-active {
  color: var(--bar-ink);
  border-bottom-color: var(--tape);
}
.right {
  display: flex;
  align-items: center;
  gap: 14px;
  margin-left: auto;
}
.tenant {
  display: flex;
  align-items: baseline;
  gap: 8px;
  font-weight: 500;
}
.tenant .label {
  color: rgb(227 232 236 / 55%);
}
.signout {
  border: 0;
  background: none;
  color: rgb(227 232 236 / 68%);
  font: 400 13px var(--font-body);
  cursor: pointer;
}
.signout:hover {
  color: var(--bar-ink);
}
main {
  max-width: 1280px;
  margin: 0 auto;
  padding: 28px 20px 64px;
}
@media (max-width: 760px) {
  /* Two rows: brand and account controls, then navigation. */
  .bar {
    flex-wrap: wrap;
    gap: 0 12px;
    height: auto;
    padding: 8px 12px 0;
  }
  nav {
    order: 3;
    width: 100%;
    height: 40px;
  }
  nav a {
    padding: 0 10px;
  }
  .tenant {
    display: none;
  }
  main {
    padding: 20px 16px 48px;
  }
}
</style>
