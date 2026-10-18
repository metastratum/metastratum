<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Backup, type Schemas, type Vm } from '../api/client'
import { backupKind, backupTone } from '../backups'
import { ago, gib } from '../state'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'
import { useSession } from '../stores/session'
import BackUpDialog from './BackUpDialog.vue'
import DeleteBackupDialog from './DeleteBackupDialog.vue'
import RestoreBackupDialog from './RestoreBackupDialog.vue'
import StatusLed from './StatusLed.vue'

const props = defineProps<{ vm: Vm }>()
const scope = useScope()
const session = useSession()

const data = ref<Schemas['VmBackups'] | null>(null)
const repos = ref<Schemas['UsableRepository'][]>([])
const error = ref('')
const busy = ref(false)
const canWrite = computed(() => scope.can('vm.write', props.vm.tenant_id))
const running = computed(() => data.value?.backups.some((b) => b.state === 'running') ?? false)

type Every = '' | 'daily' | 'weekly'
type Schedule = { every: Every; keep: number; repository_id: string; full_every: number }
const schedule = ref<Schedule>({ every: '', keep: 7, repository_id: '', full_every: 7 })
const saved = ref(false)

async function load() {
  const [d, r] = await Promise.all([
    ok(api.GET('/api/v1/vms/{id}/backups', { params: { path: { id: props.vm.id } } })),
    ok(api.GET('/api/v1/tenants/{id}/backup-repositories', { params: { path: { id: props.vm.tenant_id } } })),
  ])
  data.value = d
  repos.value = r
  const s = d.schedule
  schedule.value = s
    ? { every: s.every as Every, keep: s.keep, repository_id: s.repository_id, full_every: s.full_every }
    : { every: '', keep: 7, repository_id: r[0]?.id ?? '', full_every: 7 }
}
onMounted(load)
useLiveRefresh(['backup'], load)

const backingUp = ref(false)

async function saveSchedule() {
  error.value = ''
  saved.value = false
  busy.value = true
  try {
    const s = schedule.value
    data.value = await ok(
      api.PUT('/api/v1/vms/{id}/backup-schedule', {
        params: { path: { id: props.vm.id } },
        body: {
          every: s.every || null,
          keep: s.keep,
          repository_id: s.repository_id || null,
          full_every: s.full_every,
        },
      }),
    )
    saved.value = true
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}

const restoring = ref<Backup | null>(null)
const deleting = ref<Backup | null>(null)
</script>

<template>
  <section class="backups">
    <div class="head">
      <h2 class="label">Backups</h2>
      <button
        v-if="canWrite && repos.length"
        class="btn small"
        :disabled="busy || running || vm.status !== 'ready'"
        @click="backingUp = true"
      >
        {{ running ? 'Backing up…' : 'Back up now…' }}
      </button>
    </div>

    <p v-if="data && !repos.length" class="muted">
      No backup repository is available<template v-if="!scope.solo"> to this tenant</template>.
      <template v-if="session.me?.provider_admin">Add one under Infrastructure → Backups.</template>
      <template v-else>Ask an administrator of the tenant above to give it one.</template>
    </p>

    <form v-if="canWrite && repos.length" class="schedule" @submit.prevent="saveSchedule">
      <label class="field">
        <span class="label">Schedule</span>
        <select id="backup-every" v-model="schedule.every" @change="saved = false">
          <option value="">None</option>
          <option value="daily">Daily</option>
          <option value="weekly">Weekly</option>
        </select>
      </label>
      <label v-if="schedule.every" class="field keep">
        <span class="label">Keep</span>
        <input
          id="backup-keep"
          v-model.number="schedule.keep"
          type="number"
          min="1"
          max="365"
          required
          @input="saved = false"
        />
      </label>
      <label v-if="schedule.every" class="field keep">
        <span class="label">Full every</span>
        <input
          id="backup-full-every"
          v-model.number="schedule.full_every"
          type="number"
          min="1"
          max="365"
          required
          title="A full backup every this many; incremental ones in between (1: all full)"
          @input="saved = false"
        />
      </label>
      <label v-if="schedule.every && repos.length > 1" class="field">
        <span class="label">In</span>
        <select id="backup-repository" v-model="schedule.repository_id" @change="saved = false">
          <option v-for="r in repos" :key="r.id" :value="r.id">{{ r.name }}</option>
        </select>
      </label>
      <button class="btn small" :disabled="busy">Save schedule</button>
      <span v-if="saved" class="muted" role="status">Saved.</span>
    </form>
    <p v-else-if="data?.schedule" class="muted">
      Backed up {{ data.schedule.every }}, keeping the last {{ data.schedule.keep }};
      {{
        data.schedule.full_every === 1
          ? 'each one in full.'
          : `a full backup every ${data.schedule.full_every}, incremental ones in between.`
      }}
    </p>

    <ul v-if="data?.backups.length" class="list">
      <li v-for="b in data.backups" :key="b.id">
        <StatusLed :tone="backupTone(b)" />
        <span class="main">
          <span class="when" :title="new Date(b.created_at).toLocaleString()">{{ ago(b.created_at) }}</span>
          <span v-if="b.state === 'ready'" class="mono muted">{{ gib(b.stored_bytes ?? 0) }}</span>
        </span>
        <span class="actions">
          <button v-if="b.state === 'ready' && canWrite" class="btn small" @click="restoring = b">Restore…</button>
          <button
            v-if="b.state !== 'running' && (b.state !== 'deleting' || b.error) && canWrite"
            class="btn small danger"
            @click="deleting = b"
          >
            Delete…
          </button>
        </span>
        <span class="meta">
          <span v-if="b.state === 'running'" class="tag">Taking</span>
          <span v-if="b.state === 'deleting'" class="tag">Deleting</span>
          <span v-if="b.state !== 'failed'" class="tag" :title="backupKind(b, data.backups).title">{{
            backupKind(b, data.backups).label
          }}</span>
          <span
            v-if="b.state === 'ready'"
            class="tag"
            :title="
              b.consistent
                ? 'The VM was stopped, or its guest agent froze its file systems'
                : 'Taken without a guest agent: as after a crash, which journaling file systems recover from'
            "
            >{{ b.consistent ? 'Consistent' : 'Crash-consistent' }}</span
          >
          <span v-if="b.scheduled" class="tag">Scheduled</span>
          <span v-if="b.error" class="error-text">{{ b.error }}</span>
        </span>
      </li>
    </ul>
    <p v-else-if="data && repos.length" class="muted">No backups yet.</p>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>

    <RestoreBackupDialog v-if="restoring" :backup="restoring" @close="restoring = null" />
    <DeleteBackupDialog
      v-if="deleting && data"
      :backup="deleting"
      :backups="data.backups"
      @close="deleting = null"
    />
    <BackUpDialog
      v-if="backingUp && data"
      :vm="vm"
      :repos="repos"
      :backups="data.backups"
      @close="backingUp = false"
    />
  </section>
</template>

<style scoped>
.backups {
  display: grid;
  gap: 10px;
}
.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}
.schedule {
  display: flex;
  flex-wrap: wrap;
  align-items: end;
  gap: 8px;
}
.schedule .keep input {
  width: 72px;
}
.list {
  display: grid;
  gap: 6px;
  margin: 0;
  padding: 0;
  list-style: none;
}
.list li {
  display: grid;
  grid-template-columns: auto 1fr auto;
  align-items: center;
  gap: 4px 8px;
}
.list .main {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}
.list .actions {
  display: flex;
  gap: 6px;
}
.list .meta {
  grid-column: 2 / -1;
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.list .meta:empty {
  display: none;
}
</style>
