import { createPinia, setActivePinia } from 'pinia'
import { beforeEach, describe, expect, it } from 'vitest'
import type { Me, Tenant } from '../api/client'
import { sortTree, useScope } from './scope'
import { useSession } from './session'

const t = (id: string, parent: string | null, name: string, path: string): Tenant => ({
  id,
  parent_id: parent,
  name,
  slug: path.split('/').pop() ?? '',
  slug_path: path,
  depth: path ? path.split('/').length + 1 : 1,
  created_at: '',
})
const TENANTS = [t('acme2', 'root', 'Acme Two', 'acme2'), t('dev', 'acme', 'Dev', 'acme/dev'), t('root', null, 'Provider', ''), t('acme', 'root', 'Acme', 'acme')]

describe('scope', () => {
  beforeEach(() => setActivePinia(createPinia()))

  it('orders tenants as a tree', () => {
    expect(sortTree(TENANTS).map((x) => x.id)).toEqual(['root', 'acme', 'dev', 'acme2'])
  })

  it('inherits roles down the tree, by whole path segments', () => {
    const scope = useScope()
    scope.tenants = sortTree(TENANTS)
    useSession().me = {
      roles: [
        { tenant: 'acme', tenant_id: 'acme', tenant_name: 'Acme', role: 'operator' },
        { tenant: 'acme/dev', tenant_id: 'dev', tenant_name: 'Dev', role: 'admin' },
      ],
    } as unknown as Me
    expect(scope.roleAt('acme')).toBe('operator')
    expect(scope.roleAt('dev')).toBe('admin')
    expect(scope.roleAt('acme2')).toBeNull()
    expect(scope.roleAt('root')).toBeNull()
    expect(scope.can('vm.write', 'acme')).toBe(true)
    expect(scope.can('user.write', 'acme')).toBe(false)
    expect(scope.can('user.write', 'dev')).toBe(true)
  })
})
