import { createRouter, createWebHashHistory, createWebHistory } from 'vue-router'
import { useSession } from './stores/session'

// What the header's breadcrumb shows: the sidebar's section and the page;
// `list` names the list route a record's page belongs to.
declare module 'vue-router' {
  interface RouteMeta {
    section?: string
    title?: string
    list?: string
    public?: boolean
    provider?: boolean
  }
}

export const router = createRouter({
  // The demo is static files, served from wherever: its routes live in the URL's fragment.
  history: import.meta.env.MODE === 'demo' ? createWebHashHistory() : createWebHistory(),
  routes: [
    { path: '/login', name: 'login', component: () => import('./views/LoginView.vue'), meta: { public: true } },
    { path: '/', redirect: '/vms' },
    { path: '/vms', name: 'vms', component: () => import('./views/VmListView.vue'), meta: { section: 'Compute', title: 'Machines' } },
    // A VM's pages: its overview, consoles and settings.
    {
      path: '/vms/:id/:section(console|serial|settings)?',
      name: 'vm',
      component: () => import('./views/VmDetailView.vue'),
      props: true,
      meta: { section: 'Compute', title: 'Machines', list: 'vms' },
    },
    { path: '/disks', name: 'disks', component: () => import('./views/DisksView.vue'), meta: { section: 'Compute', title: 'Disks' } },
    { path: '/backups', name: 'backups', component: () => import('./views/BackupsView.vue'), meta: { section: 'Compute', title: 'Backups' } },
    { path: '/images', name: 'images', component: () => import('./views/ImagesView.vue'), meta: { section: 'Compute', title: 'Images' } },
    { path: '/tenants/:id?', name: 'tenants', component: () => import('./views/TenantsView.vue'), props: true, meta: { section: 'Organization', title: 'Tenants' } },
    { path: '/networks', name: 'networks', component: () => import('./views/VpcsView.vue'), meta: { section: 'Networking', title: 'VPCs' } },
    { path: '/security-groups', name: 'security-groups', component: () => import('./views/SecurityGroupsView.vue'), meta: { section: 'Networking', title: 'Security groups' } },
    { path: '/floating-ips', name: 'floating-ips', component: () => import('./views/FloatingIpsView.vue'), meta: { section: 'Networking', title: 'Floating IPs' } },
    { path: '/load-balancers', name: 'load-balancers', component: () => import('./views/LoadBalancersView.vue'), meta: { section: 'Networking', title: 'Load balancers' } },
    { path: '/guardrails', name: 'guardrails', component: () => import('./views/GuardrailsView.vue'), meta: { section: 'Networking', title: 'Guardrails' } },
    { path: '/external-networks', name: 'external-networks', component: () => import('./views/ExternalNetworksView.vue'), meta: { provider: true, section: 'Infrastructure', title: 'External networks' } },
    { path: '/audit', name: 'audit', component: () => import('./views/AuditView.vue'), meta: { section: 'Organization', title: 'Audit log' } },
    { path: '/account', name: 'account', component: () => import('./views/AccountView.vue'), meta: { title: 'Account' } },
    { path: '/infrastructure', redirect: '/hosts' },
    { path: '/hosts', name: 'hosts', component: () => import('./views/HostsView.vue'), meta: { provider: true, section: 'Infrastructure', title: 'Hosts & clusters' } },
    // Clusters are shown with their hosts.
    { path: '/clusters', name: 'clusters', redirect: '/hosts' },
    { path: '/storage', name: 'storage', component: () => import('./views/StorageView.vue'), meta: { provider: true, section: 'Infrastructure', title: 'Storage' } },
    { path: '/backup-repositories', name: 'backup-repositories', component: () => import('./views/BackupRepositoriesView.vue'), meta: { provider: true, section: 'Infrastructure', title: 'Backup repositories' } },
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
