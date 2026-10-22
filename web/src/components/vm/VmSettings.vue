<script setup lang="ts">
import { computed, ref } from 'vue'
import { api, errorText, ok, type Schemas, type Vm } from '../../api/client'

/**
 * A VM's settings, grouped: most change only while it is stopped, and apply
 * from its next start.
 */
const props = defineProps<{ vm: Vm; running: boolean }>()
const emit = defineEmits<{ update: [vm: Vm]; stop: []; delete: [] }>()

const stopped = computed(
  () => props.vm.status === 'ready' && props.vm.power === 'stopped' && props.vm.desired_power === 'stopped',
)
const LOCKED = 'Stop the VM to change this'
const error = ref('')

// Changes being saved, by the row they come from: only that row's control
// shows it (a switch in its new position). They are sent one after another,
// so each reply holds the ones before it.
const pending = ref(new Map<string, boolean | undefined>())
let saved: Promise<void> = Promise.resolve()
function patch(label: string, body: Schemas['UpdateVm'], on?: boolean) {
  if (pending.value.has(label)) return
  error.value = ''
  pending.value.set(label, on)
  saved = saved.then(async () => {
    try {
      emit('update', await ok(api.PATCH('/api/v1/vms/{id}', { params: { path: { id: props.vm.id } }, body })))
    } catch (e) {
      error.value = errorText(e)
    } finally {
      pending.value.delete(label)
    }
  })
}

// Taking a TPM away loses what it holds: asked twice.
const removingTpm = ref(false)
function pressTpm() {
  if (!props.vm.tpm) return void patch('TPM 2.0', { tpm: true })
  if (!removingTpm.value) return void (removingTpm.value = true)
  removingTpm.value = false
  void patch('TPM 2.0', { tpm: false })
}

type Row =
  | { kind: 'info'; label: string; desc?: string; value: string }
  | { kind: 'switch'; label: string; desc: string; on: boolean; toggle: () => void }
  | {
      kind: 'button'
      label: string
      desc: string
      value?: string
      action: string
      press: () => void
      blur?: () => void
      danger?: boolean
      /** Whatever the VM's state. */
      always?: boolean
      disabled?: boolean
    }

const groups = computed(() => {
  const vm = props.vm
  const security: Row[] = [{ kind: 'info', label: 'Firmware', value: vm.firmware === 'uefi' ? 'UEFI' : 'BIOS' }]
  if (vm.firmware === 'uefi') {
    security.push({
      kind: 'switch',
      label: 'Secure Boot',
      desc: 'Only signed boot loaders start. Its UEFI settings start anew at its next start.',
      on: vm.secure_boot,
      toggle: () => void patch('Secure Boot', { secure_boot: !vm.secure_boot }, !vm.secure_boot),
    })
  }
  security.push({
    kind: 'button',
    label: 'TPM 2.0',
    desc: vm.tpm
      ? 'Its keys move with the VM. Removing it loses them, for good.'
      : 'A new, empty virtual TPM, for keys and measured boot.',
    value: vm.tpm ? 'Added' : 'None',
    action: vm.tpm ? (removingTpm.value ? 'Remove it and its keys' : 'Remove') : 'Add',
    danger: vm.tpm && removingTpm.value,
    press: pressTpm,
    blur: () => (removingTpm.value = false),
  })
  if (vm.guest_os === 'windows') {
    security.push({
      kind: 'info',
      label: 'System',
      desc: vm.first_boot_pending ? 'First start pending: Windows sets its password then.' : undefined,
      value: `Windows${vm.account ? `, account ${vm.account}` : ''}`,
    })
  }
  return [
    { title: 'Firmware & security', rows: security },
    {
      title: 'Placement & CPU',
      rows: [
        {
          kind: 'switch',
          label: 'Allow live migration',
          desc: 'Off gives it the host’s CPU and disk caching.',
          on: vm.migratable,
          toggle: () => void patch('Allow live migration', { migratable: !vm.migratable }, !vm.migratable),
        },
        vm.migratable
          ? {
              kind: 'button',
              label: 'CPU model',
              desc: 'Fit to cluster picks one every host of its cluster runs now, from its next start: it may see fewer CPU features.',
              value: vm.cpu_model ?? 'Its host’s, as is',
              action: 'Fit to cluster',
              press: () => void patch('CPU model', { fit_cpu: true }),
            }
          : { kind: 'info', label: 'CPU model', desc: 'The host’s own, while live migration is off.', value: 'Host CPU' },
      ] as Row[],
    },
    {
      title: 'Memory & power',
      rows: [
        {
          kind: 'switch',
          label: 'Return free memory to the host',
          desc: vm.free_page_reporting
            ? 'The guest hands memory it frees back to the host, which can use it elsewhere.'
            : 'The guest keeps memory it frees: no faults to get it back, but the host cannot use it.',
          on: vm.free_page_reporting,
          toggle: () =>
            void patch(
              'Return free memory to the host',
              { free_page_reporting: !vm.free_page_reporting },
              !vm.free_page_reporting,
            ),
        },
        {
          kind: 'switch',
          label: 'Allow sleep',
          desc: 'The guest may suspend when idle, as desktops do; Wake wakes it.',
          on: vm.sleep,
          toggle: () => void patch('Allow sleep', { sleep: !vm.sleep }, !vm.sleep),
        },
      ] as Row[],
    },
    {
      title: 'Lifecycle',
      rows: [
        {
          kind: 'button',
          label: 'Template',
          desc: 'VMs are cloned from a template; it never starts.',
          action: vm.template ? 'Make it a VM' : 'Make template',
          press: () => void patch('Template', { template: !vm.template }),
        },
        {
          kind: 'button',
          label: 'Delete VM',
          desc: 'Its backups are kept, to restore as a new VM.',
          action: 'Delete VM…',
          danger: true,
          always: true,
          disabled: vm.status === 'deleting' || vm.status === 'migrating',
          press: () => emit('delete'),
        },
      ] as Row[],
    },
  ]
})

const locked = (r: Row) => r.kind !== 'info' && !(r.kind === 'button' && r.always) && !stopped.value
const saving = (r: Row) => pending.value.has(r.label)
const shown = (r: Row & { kind: 'switch' }) => pending.value.get(r.label) ?? r.on
</script>

<template>
  <div class="settings">
    <div v-if="!stopped && vm.status !== 'deleting'" class="locked" role="status">
      <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
        <rect x="3" y="7" width="10" height="7" rx="1" fill="none" stroke="currentColor" stroke-width="1.5" />
        <path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2" fill="none" stroke="currentColor" stroke-width="1.5" />
      </svg>
      <span>Stop {{ vm.name }} to change these. Most apply from its next start.</span>
      <button v-if="running" type="button" class="btn small" title="Ask the guest OS to shut down" @click="emit('stop')">
        Shut down
      </button>
    </div>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>

    <section v-for="g in groups" :key="g.title" class="group">
      <h2>{{ g.title }}</h2>
      <div v-for="r in g.rows" :key="r.label" class="row">
        <div class="text">
          <span class="name">{{ r.label }}</span>
          <span v-if="r.desc" class="desc">{{ r.desc }}</span>
        </div>
        <div class="control">
          <span v-if="r.kind !== 'switch' && r.value" class="value">{{ r.value }}</span>
          <button
            v-if="r.kind === 'switch'"
            type="button"
            role="switch"
            class="switch"
            :aria-checked="shown(r)"
            :aria-label="r.label"
            :aria-busy="saving(r) || undefined"
            :disabled="locked(r)"
            :title="stopped ? undefined : LOCKED"
            @click="r.toggle()"
          >
            <span class="knob" />
          </button>
          <button
            v-else-if="r.kind === 'button'"
            type="button"
            class="btn"
            :class="{ danger: r.danger }"
            :disabled="locked(r) || saving(r) || r.disabled"
            :title="r.always || stopped ? undefined : LOCKED"
            @click="r.press()"
            @blur="r.blur?.()"
          >
            {{ r.action }}
          </button>
        </div>
      </div>
    </section>
  </div>
</template>

<style scoped>
.settings {
  display: flex;
  flex-direction: column;
  gap: 16px;
  max-width: 860px;
}
.locked {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 14px;
  padding: 12px 16px;
  border-radius: var(--radius);
  background: var(--sunken);
}
.locked .btn {
  margin-left: auto;
}
.group {
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
.group h2 {
  padding: 14px 18px;
  border-bottom: 1px solid var(--rule);
  font-size: 15px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
}
.row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 12px 24px;
  padding: 14px 18px;
}
.row + .row {
  border-top: 1px solid var(--rule);
}
.text {
  display: grid;
  flex: 1 1 340px;
  gap: 2px;
  min-width: 0;
}
.name {
  font-weight: 500;
}
.desc {
  font-size: 13px;
  color: var(--muted);
  text-wrap: pretty;
}
.control {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: flex-end;
  gap: 12px;
}
.value {
  font-size: 13px;
  text-align: right;
}
.btn {
  white-space: nowrap;
}
/* A switch is named for what it turns on; its position is the state. */
.switch {
  position: relative;
  flex: none;
  width: 40px;
  height: 22px;
  padding: 0;
  border: 0;
  border-radius: 11px;
  background: var(--led-off);
  cursor: pointer;
}
.switch[aria-checked='true'] {
  background: var(--ink);
}
.switch:disabled {
  opacity: 0.45;
  cursor: default;
}
.knob {
  position: absolute;
  top: 3px;
  left: 3px;
  width: 16px;
  height: 16px;
  border-radius: 50%;
  background: var(--panel);
  transition: left 120ms;
}
.switch[aria-checked='true'] .knob {
  left: 21px;
  background: var(--tape);
}
@media (prefers-reduced-motion: reduce) {
  .knob {
    transition: none;
  }
}
</style>
