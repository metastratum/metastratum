import { createRouter, createWebHashHistory, createWebHistory } from 'vue-router'
import { useSession } from './stores/session'

export const router = createRouter({
  // The demo is static files, served from wherever: its routes live in the URL's fragment.
  history: import.meta.env.MODE === 'demo' ? createWebHashHistory() : createWebHistory(),
  routes: [
    { path: '/login', name: 'login', component: () => import('./views/LoginView.vue'), meta: { public: true } },
    { path: '/', redirect: '/vms' },
    { path: '/vms', name: 'vms', component: () => import('./views/VmListView.vue') },
    { path: '/vms/:id', name: 'vm', component: () => import('./views/VmDetailView.vue'), props: true },
    { path: '/disks', name: 'disks', component: () => import('./views/DisksView.vue'), meta: { machines: true } },
    { path: '/backups', name: 'backups', component: () => import('./views/BackupsView.vue'), meta: { machines: true } },
    { path: '/images', name: 'images', component: () => import('./views/ImagesView.vue') },
    { path: '/tenants/:id?', name: 'tenants', component: () => import('./views/TenantsView.vue'), props: true },
    { path: '/networks', name: 'networks', component: () => import('./views/VpcsView.vue'), meta: { net: true } },
    { path: '/security-groups', name: 'security-groups', component: () => import('./views/SecurityGroupsView.vue'), meta: { net: true } },
    { path: '/floating-ips', name: 'floating-ips', component: () => import('./views/FloatingIpsView.vue'), meta: { net: true } },
    { path: '/load-balancers', name: 'load-balancers', component: () => import('./views/LoadBalancersView.vue'), meta: { net: true } },
    { path: '/guardrails', name: 'guardrails', component: () => import('./views/GuardrailsView.vue'), meta: { net: true } },
    { path: '/external-networks', name: 'external-networks', component: () => import('./views/ExternalNetworksView.vue'), meta: { provider: true, infra: true } },
    { path: '/audit', name: 'audit', component: () => import('./views/AuditView.vue') },
    { path: '/account', name: 'account', component: () => import('./views/AccountView.vue') },
    { path: '/infrastructure', redirect: '/hosts' },
    { path: '/hosts', name: 'hosts', component: () => import('./views/HostsView.vue'), meta: { provider: true, infra: true } },
    { path: '/clusters', name: 'clusters', component: () => import('./views/ClustersView.vue'), meta: { provider: true, infra: true } },
    { path: '/storage', name: 'storage', component: () => import('./views/StorageView.vue'), meta: { provider: true, infra: true } },
    { path: '/backup-repositories', name: 'backup-repositories', component: () => import('./views/BackupRepositoriesView.vue'), meta: { provider: true, infra: true } },
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
