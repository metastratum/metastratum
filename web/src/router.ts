import { createRouter, createWebHistory } from 'vue-router'
import { useSession } from './stores/session'

export const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/login', name: 'login', component: () => import('./views/LoginView.vue'), meta: { public: true } },
    { path: '/', redirect: '/vms' },
    { path: '/vms', name: 'vms', component: () => import('./views/VmListView.vue') },
    { path: '/vms/:id', name: 'vm', component: () => import('./views/VmDetailView.vue'), props: true },
    { path: '/images', name: 'images', component: () => import('./views/ImagesView.vue') },
    { path: '/tenants/:id?', name: 'tenants', component: () => import('./views/TenantsView.vue'), props: true },
    { path: '/audit', name: 'audit', component: () => import('./views/AuditView.vue') },
    { path: '/account', name: 'account', component: () => import('./views/AccountView.vue') },
    { path: '/infrastructure', redirect: '/hosts' },
    { path: '/hosts', name: 'hosts', component: () => import('./views/HostsView.vue'), meta: { provider: true, infra: true } },
    { path: '/clusters', name: 'clusters', component: () => import('./views/ClustersView.vue'), meta: { provider: true, infra: true } },
    { path: '/storage', name: 'storage', component: () => import('./views/StorageView.vue'), meta: { provider: true, infra: true } },
    { path: '/:rest(.*)*', name: 'missing', component: () => import('./views/NotFoundView.vue') },
  ],
})

router.beforeEach(async (to) => {
  const session = useSession()
  if (!session.checked) await session.load()
  if (to.meta.public) return session.me && to.name === 'login' ? { name: 'vms' } : true
  if (!session.me) return { name: 'login', query: to.fullPath !== '/vms' ? { next: to.fullPath } : {} }
  if (to.meta.provider && !session.me.provider_admin) return { name: 'vms' }
  return true
})
