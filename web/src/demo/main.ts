// The demo's entry: the backend in the page first, then the app as it is.
import './install'
import '../main'
import { router } from '../router'
import { reset } from './store'

// Says it is a demo, and how to start over; on the sign-in page, who to
// sign in as.
const tag = document.createElement('div')
tag.className = 'demo-tag'
const text = document.createElement('span')
const say = () => {
  text.innerHTML = router.currentRoute.value.name === 'login'
    ? `Sign in as <code>admin</code>, or <code>alice</code> at tenant <code>northwind</code>, or <code>gina</code> at
       <code>globex</code>; password <code>demo</code>.`
    : 'Runs in your browser; your changes stay there.'
}
say()
router.afterEach(say)
tag.innerHTML = '<strong>Demo</strong>'
tag.append(text)
const button = document.createElement('button')
button.type = 'button'
button.textContent = 'Reset'
button.title = 'Forget your changes and start again'
button.onclick = () => reset()
tag.append(button)
document.body.append(tag)

const style = document.createElement('style')
style.textContent = `
.demo-tag {
  position: fixed; left: 12px; bottom: 12px; z-index: 50; max-width: min(420px, calc(100vw - 24px));
  display: flex; align-items: center; gap: 10px; padding: 6px 6px 6px 10px;
  background: var(--tape); color: var(--tape-ink); border-radius: var(--radius); box-shadow: var(--shadow);
  font: 12px/1.35 var(--font-body);
}
.demo-tag strong { font: 600 12px var(--font-label); letter-spacing: .08em; text-transform: uppercase; }
.demo-tag code { font: 11px var(--font-mono); }
.demo-tag button {
  flex: none; border: 1px solid currentColor; background: transparent; color: inherit; border-radius: var(--radius);
  font: 600 11px var(--font-label); letter-spacing: .06em; text-transform: uppercase; padding: 4px 8px; cursor: pointer;
}
.demo-tag button:hover { background: rgb(0 0 0 / 8%); }
.demo-tag button:focus-visible { outline: 2px solid var(--tape-ink); outline-offset: 2px; }
`
document.head.append(style)
