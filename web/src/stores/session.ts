import { defineStore } from 'pinia'
import { api, ok, type Me } from '../api/client'

export const useSession = defineStore('session', {
  state: () => ({ me: null as Me | null, checked: false }),
  actions: {
    async load() {
      const { data, response } = await api.GET('/api/v1/auth/me')
      this.me = response.ok ? (data ?? null) : null
      this.checked = true
      return this.me
    },
    async login(username: string, password: string, tenant: string) {
      await ok(api.POST('/api/v1/auth/login', { body: { username, password, tenant: tenant.trim() || null } }))
      return this.load()
    },
    async logout() {
      await api.POST('/api/v1/auth/logout')
      this.me = null
    },
  },
})
