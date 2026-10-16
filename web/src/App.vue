<script setup lang="ts">
import { watch } from 'vue'
import { useRouter } from 'vue-router'
import TaskTray from './components/TaskTray.vue'
import TenantPicker from './components/TenantPicker.vue'
import { onLive, useLive } from './stores/live'
import { useScope } from './stores/scope'
import { useSession } from './stores/session'
import { useTasks } from './stores/tasks'

const session = useSession()
const scope = useScope()
const tasks = useTasks()
const live = useLive()
const router = useRouter()

// The tenant tree changes when tenants are added, renamed or removed.
onLive(['tenant'], () => session.me && void scope.load())

watch(
  () => session.me,
  (me) => {
    if (me) {
      live.start()
      tasks.start()
      void scope.load()
    } else {
      live.stop()
      tasks.stop()
    }
  },
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
        <RouterLink to="/networks" :class="{ 'router-link-active': $route.meta.net }">Networks</RouterLink>
        <RouterLink to="/tenants">Tenants</RouterLink>
        <RouterLink v-if="scope.can('audit.read')" to="/audit">Audit</RouterLink>
        <RouterLink v-if="session.me.provider_admin" to="/infrastructure" :class="{ 'router-link-active': $route.meta.infra }">
          Infrastructure
        </RouterLink>
      </nav>
      <div class="right">
        <TenantPicker class="tenant" />
        <TaskTray />
        <RouterLink to="/account" class="account" :title="`Signed in as ${session.me.username}`">
          {{ session.me.display_name || session.me.username }}
        </RouterLink>
        <button type="button" class="signout" @click="signOut">Sign out</button>
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
.account {
  max-width: 160px;
  overflow: hidden;
  color: var(--bar-ink);
  font-weight: 500;
  text-decoration: none;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.account:hover,
.account.router-link-active {
  text-decoration: underline;
  text-underline-offset: 4px;
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
  nav {
    overflow-x: auto;
  }
  .tenant :deep(.label),
  .account {
    display: none;
  }
  main {
    padding: 20px 16px 48px;
  }
}
</style>
