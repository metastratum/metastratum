<script setup lang="ts">
import { computed } from 'vue'
import { PAGE_SIZES } from '../paging'

/** Where a paged list is, and the way to its other pages (see usePaging). */
const props = defineProps<{ total: number }>()
const page = defineModel<number>('page', { required: true })
const perPage = defineModel<number>('perPage', { required: true })

const first = computed(() => (page.value - 1) * perPage.value + 1)
const last = computed(() => Math.min(props.total, page.value * perPage.value))
const pages = computed(() => Math.max(1, Math.ceil(props.total / perPage.value)))
</script>

<template>
  <div v-if="total" class="pager">
    <span class="muted">Showing {{ first }}–{{ last }} of {{ total }}</span>
    <label class="size">
      <span class="muted">Per page</span>
      <select v-model.number="perPage">
        <option v-for="n in PAGE_SIZES" :key="n" :value="n">{{ n }}</option>
      </select>
    </label>
    <div class="steps">
      <button type="button" class="btn small" :disabled="page <= 1" @click="page--">Previous</button>
      <button type="button" class="btn small" :disabled="page >= pages" @click="page++">Next</button>
    </div>
  </div>
</template>

<style scoped>
.pager {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px 16px;
}
.size {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-left: auto;
  font-size: 13px;
}
select {
  height: 30px;
  padding: 0 6px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  color: var(--ink);
  font: 400 13px var(--font-body);
}
.steps {
  display: flex;
  gap: 6px;
}
.steps .btn {
  height: 30px;
  padding: 0 10px;
  font-size: 13px;
}
</style>
