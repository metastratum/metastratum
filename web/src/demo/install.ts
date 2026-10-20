// Puts the demo backend in place of the network: the API's requests,
// its event stream and the console sockets are answered in the page.
// Imported before the app, and the shim first of all.
import { plug } from './shim'
import { ConsoleSocket, isConsoleUrl } from './console'
import { FakeEventSource } from './events'
import './handlers'
import { handle, onRequestDone } from './http'
import { save } from './store'
import { resumeTasks } from './tasks'

plug(handle)

window.EventSource = FakeEventSource as unknown as typeof EventSource

const RealSocket = window.WebSocket
window.WebSocket = new Proxy(RealSocket, {
  construct(target, args: [string | URL, (string | string[])?]) {
    return isConsoleUrl(String(args[0])) ? (new ConsoleSocket(String(args[0])) as unknown as WebSocket) : new target(...args)
  },
})

onRequestDone(save)
resumeTasks()
