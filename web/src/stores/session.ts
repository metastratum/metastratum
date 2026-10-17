import { defineStore } from 'pinia'
import { api, ok, type Me, type SecondFactorChallenge } from '../api/client'

export const useSession = defineStore('session', {
  state: () => ({ me: null as Me | null, checked: false }),
  actions: {
    async load() {
      const { data, response } = await api.GET('/api/v1/auth/me')
      this.me = response.ok ? (data ?? null) : null
      this.checked = true
      return this.me
    },
    /** The password step. Returns the second factor's challenge when one
     *  is to come; else the user is signed in. */
    async login(username: string, password: string, tenant: string): Promise<SecondFactorChallenge | null> {
      const next = await ok(api.POST('/api/v1/auth/login', { body: { username, password, tenant: tenant.trim() || null } }))
      if (next?.second_factor) return next.second_factor
      await this.load()
      return null
    },
    /** The second step: a typed code, or a security key's answer. */
    async secondFactor(token: string, answer: { code?: string; webauthn?: Record<string, unknown> }) {
      await ok(api.POST('/api/v1/auth/login/second-factor', { body: { token, ...answer } as never }))
      return this.load()
    },
    async logout() {
      await api.POST('/api/v1/auth/logout')
      this.me = null
    },
  },
})
