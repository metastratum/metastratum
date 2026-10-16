import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from './App.vue'
import { router } from './router'
import { setUnauthenticatedHandler } from './api/client'
import { useSession } from './stores/session'
import './styles/base.css'

const app = createApp(App).use(createPinia()).use(router)

// A session that expires mid-use sends the user back to sign in.
setUnauthenticatedHandler(() => {
  useSession().me = null
  if (router.currentRoute.value.name !== 'login') {
    void router.push({ name: 'login', query: { next: router.currentRoute.value.fullPath } })
  }
})

app.mount('#app')
