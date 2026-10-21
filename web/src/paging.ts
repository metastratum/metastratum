import { computed, ref, watch, type Ref } from 'vue'

export const PAGE_SIZES = [50, 100, 200] as const

/**
 * A list shown a page at a time, back to the first page whenever what it
 * holds changes in length (another filter, say).
 */
export function usePaging<T>(items: Ref<T[]>) {
  const page = ref(1)
  const perPage = ref<number>(PAGE_SIZES[0])
  const pages = computed(() => Math.max(1, Math.ceil(items.value.length / perPage.value)))
  watch([() => items.value.length, perPage], () => (page.value = 1))
  watch(pages, (n) => page.value > n && (page.value = n))
  const shown = computed(() => items.value.slice((page.value - 1) * perPage.value, page.value * perPage.value))
  return { page, perPage, pages, shown }
}
