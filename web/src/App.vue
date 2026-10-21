<script setup lang="ts">
import { computed, watch } from 'vue'
import { useRouter } from 'vue-router'
import { useRoute } from 'vue-router'
import AppSidebar from './components/AppSidebar.vue'
import BrandMark from './components/BrandMark.vue'
import TaskTray from './components/TaskTray.vue'
import TwoFactorPanel from './components/TwoFactorPanel.vue'
import { onLive, useLive } from './stores/live'
import { useScope } from './stores/scope'
import { useSession } from './stores/session'
import { useShell } from './stores/shell'
import { useTasks } from './stores/tasks'

const session = useSession()
const scope = useScope()
const tasks = useTasks()
const live = useLive()
const router = useRouter()
const route = useRoute()
const shell = useShell()

// A page names its record (a VM's name) once it has it; another page, none.
watch(
  () => route.name,
  () => (shell.crumb = null),
)

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
      // Whoever signs in next starts from their own tenants, not these.
      scope.$reset()
      shell.$reset()
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
  <div v-else-if="session.me" class="shell">
    <AppSidebar @sign-out="signOut" />
    <div class="column">
      <header class="head">
        <nav aria-label="Breadcrumb" class="crumbs label">
          <template v-if="route.meta.section">
            <span>{{ route.meta.section }}</span>
            <span aria-hidden="true">/</span>
          </template>
          <template v-if="shell.crumb && route.meta.list">
            <RouterLink :to="{ name: route.meta.list }">{{ route.meta.title }}</RouterLink>
            <span aria-hidden="true">/</span>
            <span class="here name">{{ shell.crumb }}</span>
          </template>
          <span v-else class="here">{{ route.meta.title }}</span>
        </nav>
        <TaskTray class="tasks" />
      </header>
      <main>
        <RouterView />
      </main>
    </div>
  </div>
  <RouterView v-else />
</template>

<style scoped>
/* Signed in, but a second factor is to be added first: no way around. */
.bar {
  display: flex;
  align-items: center;
  gap: 28px;
  height: 52px;
  padding: 0 20px;
  background: var(--bar);
  color: var(--bar-ink);
}
.right {
  display: flex;
  align-items: center;
  gap: 14px;
  margin-left: auto;
}
.me {
  color: var(--bar-ink);
  font-weight: 500;
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
  padding: 28px 20px 64px;
}

/* The sidebar beside the page, which has the breadcrumb above it. */
.shell {
  display: grid;
  grid-template-columns: 232px minmax(0, 1fr);
  min-height: 100vh;
}
.shell > :deep(.sidebar) {
  position: sticky;
  top: 0;
  height: 100vh;
  overflow-y: auto;
}
.column {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.head {
  position: sticky;
  top: 0;
  z-index: 10;
  display: flex;
  align-items: center;
  gap: 12px;
  min-height: 52px;
  padding: 0 32px;
  border-bottom: 1px solid var(--rule);
  background: var(--panel);
}
.crumbs {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
  font-size: 12px;
}
.crumbs a {
  color: var(--muted);
  text-decoration: none;
}
.crumbs a:hover {
  color: var(--ink);
}
.here {
  color: var(--ink);
}
/* A record's own name, as it is written. */
.here.name {
  overflow: hidden;
  text-overflow: ellipsis;
  text-transform: none;
  letter-spacing: 0.02em;
  white-space: nowrap;
}
.tasks {
  margin-left: auto;
}
main {
  width: 100%;
  max-width: 1280px;
  padding: 28px 32px 64px;
}
@media (max-width: 760px) {
  /* The sidebar above the page, its sections side by side. */
  .shell {
    grid-template-columns: minmax(0, 1fr);
  }
  .shell > :deep(.sidebar) {
    position: static;
    height: auto;
  }
  .shell > :deep(.sidebar) nav {
    flex-flow: row wrap;
    gap: 12px 18px;
  }
  .shell > :deep(.sidebar) .group {
    flex: 1 1 140px;
  }
  .shell > :deep(.sidebar) .group.infra {
    padding-top: 0;
    border-top: 0;
  }
  .head,
  main {
    padding-inline: 16px;
  }
  main {
    padding-block: 20px 48px;
  }
}
</style>
