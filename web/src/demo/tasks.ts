// Tasks, as the controller's engine runs them: a row that goes from
// pending through its steps to succeeded or failed, announced as it goes.
// Each kind has a driver: its steps (what the task tray shows, and how long
// each takes here), and what it does (at the start, and once done). A task
// cut short by a reload goes on from its step when the demo loads again.
import type { components } from '../api/schema'
import { emit } from './events'
import { db, newId, now, save, type TaskRec } from './store'

export type Driver = {
  /** Each step's description and how long it takes, in ms. */
  steps: (t: TaskRec) => [string, number][]
  /** Runs as the task starts (before its first step). */
  begin?: (t: TaskRec) => void
  /** Runs before each step (by index); may throw to fail the task. */
  step?: (t: TaskRec, i: number) => void
  /** Runs once all steps are done; may throw to fail the task. */
  finish: (t: TaskRec) => void
  /** Runs when the task failed (to undo `begin`). */
  failed?: (t: TaskRec, error: string) => void
}

const drivers = new Map<string, Driver>()

/** Registers how tasks of a kind run. */
export function driver(kind: string, d: Driver) {
  drivers.set(kind, d)
}

type Start = {
  kind: string
  tenant_id: string
  target_kind?: string | null
  target_id?: string | null
  params?: Record<string, unknown>
}

/** Starts a task; it runs in the background. */
export function startTask(s: Start): TaskRec {
  const t: TaskRec = {
    id: newId(),
    kind: s.kind,
    tenant_id: s.tenant_id,
    target_kind: s.target_kind ?? null,
    target_id: s.target_id ?? null,
    state: 'pending',
    step: null,
    progress: null,
    error: null,
    created_at: now(),
    started_at: null,
    finished_at: null,
    params: s.params ?? {},
    at: -1,
  }
  db.tasks.unshift(t)
  // The newest few hundred are plenty for a demo.
  if (db.tasks.length > 300) db.tasks.length = 300
  emit('task', t.id, 'insert')
  setTimeout(() => run(t), 300)
  return t
}

/** The task a record is busy with, if any. */
export function activeTask(targetId: string): TaskRec | undefined {
  return db.tasks.find((t) => t.target_id === targetId && (t.state === 'pending' || t.state === 'running'))
}

function changed(t: TaskRec) {
  emit('task', t.id)
  if (t.target_id && t.target_kind) emit(t.target_kind as never, t.target_id)
  save()
}

function fail(t: TaskRec, d: Driver | undefined, e: unknown) {
  const error = e instanceof Error ? e.message : String(e)
  t.state = 'failed'
  t.error = error
  t.finished_at = now()
  try {
    d?.failed?.(t, error)
  } catch (e2) {
    console.error('[demo] undoing a failed task', e2)
  }
  changed(t)
}

function run(t: TaskRec) {
  const d = drivers.get(t.kind)
  if (!d) return fail(t, d, new Error(`no driver for ${t.kind}`))
  if (t.at < 0) {
    t.state = 'running'
    t.started_at = now()
    t.at = 0
    try {
      d.begin?.(t)
    } catch (e) {
      return fail(t, d, e)
    }
  }
  const steps = d.steps(t)
  const next = () => {
    if (t.state !== 'running') return
    if (t.at >= steps.length) {
      try {
        d.finish(t)
      } catch (e) {
        return fail(t, d, e)
      }
      t.state = 'succeeded'
      t.progress = 1
      t.step = null
      t.finished_at = now()
      return changed(t)
    }
    const [label, ms] = steps[t.at]!
    try {
      d.step?.(t, t.at)
    } catch (e) {
      return fail(t, d, e)
    }
    t.step = label
    t.progress = t.at / steps.length
    changed(t)
    setTimeout(() => {
      t.at++
      next()
    }, ms)
  }
  next()
}

/** Picks up tasks a reload cut short. */
export function resumeTasks() {
  for (const t of db.tasks) {
    if (t.state === 'pending' || t.state === 'running') setTimeout(() => run(t), 500)
  }
}

export type TaskView = components['schemas']['TaskView']
/** A task as the API shows it. */
export function taskView(t: TaskRec): TaskView {
  const { params: _p, at: _a, ...view } = t
  return view
}
