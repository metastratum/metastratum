// Minimal typings for the parts of noVNC we use.
declare module '@novnc/novnc' {
  export default class RFB extends EventTarget {
    constructor(target: HTMLElement, url: string, options?: { wsProtocols?: string[]; shared?: boolean })
    scaleViewport: boolean
    resizeSession: boolean
    focusOnClick: boolean
    background: string
    disconnect(): void
    sendCtrlAltDel(): void
    focus(options?: FocusOptions): void
  }
}
