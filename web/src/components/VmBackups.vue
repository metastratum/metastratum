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

const EVERY: { value: Every; label: string }[] = [
  { value: '', label: 'None' },
  { value: 'daily', label: 'Daily' },
  { value: 'weekly', label: 'Weekly' },
]
function pickEvery(e: Every) {
  schedule.value.every = e
  saved.value = false
}

// A few at first; all of them, scrolling, on request.
const FEW = 3
const all = ref(false)
const shown = computed(() => (all.value ? data.value?.backups : data.value?.backups.slice(0, FEW)) ?? [])

const restoring = ref<Backup | null>(null)
const deleting = ref<Backup | null>(null)
</script>

<template>
  <div class="backups">
    <div class="vm-sub-head">
      <span class="label">Backups</span>
      <button
        v-if="canWrite && repos.length"
        type="button"
        class="btn small"
        :disabled="busy || running || vm.status !== 'ready'"
        @click="backingUp = true"
      >
        {{ running ? 'Backing up…' : 'Back up now…' }}
      </button>
    </div>

    <p v-if="data && !repos.length" class="vm-none">
      No backup repository is available<template v-if="!scope.solo"> to this tenant</template>.
      <template v-if="session.me?.provider_admin">Add one under Infrastructure → Backup repositories.</template>
      <template v-else>Ask an administrator of the tenant above to give it one.</template>
    </p>

    <form v-if="canWrite && repos.length" class="schedule" @submit.prevent="saveSchedule">
      <div class="line">
        <span class="what">Schedule</span>
        <div class="segmented" role="radiogroup" aria-label="Back up">
          <button
            v-for="o in EVERY"
            :id="o.value ? `backup-every-${o.value}` : 'backup-every-none'"
            :key="o.value"
            type="button"
            role="radio"
            :aria-checked="schedule.every === o.value"
            :class="{ on: schedule.every === o.value }"
            @click="pickEvery(o.value)"
          >
            {{ o.label }}
          </button>
        </div>
      </div>
      <template v-if="schedule.every">
        <label class="line">
          <span class="key">Retention</span>
          <span>Keep the last</span>
          <input
            id="backup-keep"
            v-model.number="schedule.keep"
            type="number"
            min="1"
            max="365"
            required
            aria-label="Backups to keep"
            @input="saved = false"
          />
          <span>backups</span>
        </label>
        <label class="line">
          <span class="key">Full backups</span>
          <span>One in every</span>
          <input
            id="backup-full-every"
            v-model.number="schedule.full_every"
            type="number"
            min="1"
            max="365"
            required
            aria-label="Full backup every"
            title="A full backup every this many; incremental ones in between (1: all full)"
            @input="saved = false"
          />
          <span>backups is full</span>
        </label>
        <label v-if="repos.length > 1" class="line">
          <span class="key">Repository</span>
          <select id="backup-repository" v-model="schedule.repository_id" @change="saved = false">
            <option v-for="r in repos" :key="r.id" :value="r.id">{{ r.name }}</option>
          </select>
        </label>
      </template>
      <div class="line">
        <button class="btn small" :disabled="busy">Save schedule</button>
        <span v-if="saved" class="muted" role="status">Saved.</span>
      </div>
    </form>
    <p v-else-if="data?.schedule" class="muted">
      Backed up {{ data.schedule.every }}, keeping the last {{ data.schedule.keep }};
      {{
        data.schedule.full_every === 1
          ? 'each one in full.'
          : `a full backup every ${data.schedule.full_every}, incremental ones in between.`
      }}
    </p>

    <div v-if="data?.backups.length" class="vm-rows">
      <ul :class="{ all }">
        <li v-for="b in shown" :key="b.id">
          <StatusLed :tone="backupTone(b)" />
          <span :title="new Date(b.created_at).toLocaleString()">{{ ago(b.created_at) }}</span>
          <span v-if="b.state === 'ready'" class="mono muted">{{ gib(b.stored_bytes ?? 0) }}</span>
          <span class="end">
            <button
              v-if="b.state === 'ready' && canWrite"
              type="button"
              class="btn small"
              title="Restore it as a new VM"
              @click="restoring = b"
            >
              Restore…
            </button>
            <button
              v-if="b.state !== 'running' && (b.state !== 'deleting' || b.error) && canWrite"
              type="button"
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
      <button v-if="data.backups.length > FEW" type="button" class="more-rows" @click="all = !all">
        {{ all ? 'Show fewer' : `Show ${data.backups.length - FEW} more backups` }}
      </button>
    </div>
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
  </div>
</template>

<style scoped>
.backups {
  display: grid;
  align-content: start;
  gap: 10px;
}
.schedule {
  display: grid;
  gap: 12px;
  padding: 14px;
  border-radius: var(--radius);
  background: var(--aisle);
}
.line {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 12px;
}
label.line {
  gap: 8px;
}
.what {
  font-weight: 500;
}
.key {
  min-width: 96px;
  color: var(--muted);
}
.line input {
  width: 64px;
  height: 30px;
  padding: 0 8px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  color: var(--ink);
  font: 400 11.5px var(--font-mono);
}
.line select {
  height: 30px;
  padding: 0 8px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  color: var(--ink);
  font: 400 13px var(--font-body);
}
.segmented {
  display: flex;
  gap: 2px;
  padding: 3px;
  border-radius: var(--radius);
  background: var(--sunken);
}
.segmented button {
  height: 26px;
  padding: 0 12px;
  border: 0;
  border-radius: 2px;
  background: transparent;
  color: var(--muted);
  font: 500 13px var(--font-body);
  cursor: pointer;
}
.segmented button:hover {
  color: var(--ink);
}
.segmented button.on {
  background: var(--panel);
  color: var(--ink);
  box-shadow: 0 1px 0 rgb(28 35 41 / 6%);
}
</style>
