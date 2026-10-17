<script setup lang="ts">
import { computed, watch } from 'vue'
import { useRouter } from 'vue-router'
import BrandMark from './components/BrandMark.vue'
import TaskTray from './components/TaskTray.vue'
import TwoFactorPanel from './components/TwoFactorPanel.vue'
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

// Signed in where a second factor is required, without one: adding one is
// all the controller allows until then.
const enrolling = computed(() => !!session.me?.two_factor_needed)

watch(
  () => session.me && !session.me.two_factor_needed,
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
  <template v-if="session.me && enrolling">
    <header class="bar">
      <span class="mark"><BrandMark bar /></span>
      <div class="right">
        <span class="me">{{ session.me.display_name || session.me.username }}</span>
        <button type="button" class="signout" @click="signOut">Sign out</button>
      </div>
    </header>
    <main class="enroll">
      <TwoFactorPanel required @changed="session.load()" />
    </main>
  </template>
  <template v-else-if="session.me">
    <header class="bar">
      <RouterLink to="/vms" class="mark" aria-label="metastratum home">
        <BrandMark bar />
      </RouterLink>
      <nav aria-label="Main">
        <RouterLink to="/vms" :class="{ 'router-link-active': $route.meta.machines }">Machines</RouterLink>
        <RouterLink to="/images">Images</RouterLink>
        <RouterLink to="/networks" :class="{ 'router-link-active': $route.meta.net }">Networks</RouterLink>
        <RouterLink to="/tenants">{{ scope.solo ? 'People' : 'Tenants' }}</RouterLink>
        <RouterLink v-if="scope.can('audit.read')" to="/audit">Audit</RouterLink>
        <RouterLink v-if="session.me.provider_admin" to="/infrastructure" :class="{ 'router-link-active': $route.meta.infra }">
          Infrastructure
        </RouterLink>
      </nav>
      <div class="right">
        <TenantPicker class="tenant" />
        <TaskTray />
        <RouterLink to="/account" class="me" :title="`Signed in as ${session.me.username}`">
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
.me {
  max-width: 160px;
  overflow: hidden;
  color: var(--bar-ink);
  font-weight: 500;
  text-decoration: none;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.me:hover,
.me.router-link-active {
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
main.enroll {
  max-width: 560px;
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
  .me {
    display: none;
  }
  main {
    padding: 20px 16px 48px;
  }
}
</style>
