import { defineStore } from 'pinia'

/**
 * What pages tell the frame around them: the name of the record a page
 * shows (the breadcrumb's last part), and how many VMs the list holds (shown
 * beside Machines in the sidebar).
 */
export const useShell = defineStore('shell', {
  state: () => ({ crumb: null as string | null, vmCount: null as number | null }),
})
