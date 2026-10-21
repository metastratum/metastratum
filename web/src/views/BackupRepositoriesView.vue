<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { api, errorText, ok, type BackupRepository } from '../api/client'
import ModalDialog from '../components/ModalDialog.vue'
import { ago } from '../state'
import { useLiveRefresh } from '../stores/live'

const repos = ref<BackupRepository[] | null>(null)
const error = ref('')

async function load() {
  repos.value = await ok(api.GET('/api/v1/backup-repositories'))
}
onMounted(load)
useLiveRefresh(['backup_repository'], load)

type Dialog = { kind: 'add' } | { kind: 'delete'; repo: BackupRepository }
const dialog = ref<Dialog | null>(null)
const form = ref({ name: '', server: '', export: '', options: '' })
const busy = ref(false)

function add() {
  form.value = { name: '', server: '', export: '', options: '' }
  error.value = ''
  dialog.value = { kind: 'add' }
}

async function submit() {
  const d = dialog.value
  if (!d) return
  error.value = ''
  busy.value = true
  try {
    if (d.kind === 'delete') {
      await ok(api.DELETE('/api/v1/backup-repositories/{id}', { params: { path: { id: d.repo.id } } }))
    } else {
      const f = form.value
      await ok(
        api.POST('/api/v1/backup-repositories', {
          body: { name: f.name, server: f.server, export: f.export, options: f.options || null },
        }),
      )
    }
    dialog.value = null
    await load()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <section>
    <header class="page-head">
      <h1>Backup repositories</h1>
      <span v-if="repos" class="mono muted">{{ repos.length }}</span>
      <div class="actions"><button class="btn primary" @click="add()">Add repository</button></div>
    </header>
    <p class="intro">NFS exports where VM backups are kept, apart from the storage VMs run on.</p>

    <div v-if="repos && !repos.length" class="empty-state">
      <p>No backup repositories. Without one, VMs can’t be backed up.</p>
      <button class="btn primary" @click="add()">Add repository</button>
    </div>
    <div v-else-if="repos" class="table-wrap">
      <table class="data">
        <thead>
          <tr>
            <th>Name</th>
            <th>Export</th>
            <th>Added</th>
            <th />
          </tr>
        </thead>
        <tbody>
          <tr v-for="r in repos" :key="r.id">
            <td>
              <strong>{{ r.name }}</strong>
            </td>
            <td class="mono">{{ r.server }}:{{ r.export }}</td>
            <td class="muted">{{ ago(r.created_at) }}</td>
            <td class="end">
              <button class="btn small danger" @click="((error = ''), (dialog = { kind: 'delete', repo: r }))">
                Delete
              </button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <ModalDialog
      v-if="dialog"
      :title="dialog.kind === 'delete' ? 'Delete backup repository' : 'Add backup repository'"
      @close="dialog = null"
    >
      <form class="form" @submit.prevent="submit">
        <template v-if="dialog.kind === 'delete'">
          <p>Delete <strong>{{ dialog.repo.name }}</strong>?</p>
          <ul class="consequences">
            <li>Only a repository without backups can go: delete those first.</li>
            <li>Hosts unmount it; the files on the server stay.</li>
            <li>VM schedules that use it stop.</li>
          </ul>
        </template>
        <template v-else>
          <label class="field">
            <span class="label">Name</span>
            <input id="repo-name" v-model="form.name" required maxlength="64" autofocus />
          </label>
          <div class="row">
            <label class="field">
              <span class="label">NFS server</span>
              <input id="repo-server" v-model="form.server" required class="mono" placeholder="10.0.0.40" />
            </label>
            <label class="field">
              <span class="label">Export</span>
              <input id="repo-export" v-model="form.export" required class="mono" placeholder="/srv/backups" />
            </label>
          </div>
          <label class="field">
            <span class="label">Mount options</span>
            <input id="repo-options" v-model="form.options" class="mono" placeholder="vers=4.2,hard,noatime" />
          </label>
          <p class="muted">
            QEMU writes backups as root: export it with <span class="mono">no_root_squash</span> to the hosts.
          </p>
        </template>
        <p v-if="error" class="error-text" role="alert">{{ error }}</p>
        <footer>
          <button type="button" class="btn" @click="dialog = null">Cancel</button>
          <button class="btn" :class="dialog.kind === 'delete' ? 'danger' : 'primary'" :disabled="busy">
            {{ dialog.kind === 'delete' ? 'Delete' : 'Add repository' }}
          </button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.consequences {
  display: grid;
  gap: 6px;
  margin: 0;
  padding-left: 18px;
  font-size: 13px;
}
</style>
