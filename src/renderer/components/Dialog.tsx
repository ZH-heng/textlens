import { createPortal } from 'react-dom'
import { useLayoutEffect, useRef, type JSX, type ReactNode, type RefObject } from 'react'

interface DialogProps {
  children: ReactNode
  className?: string
  label?: string
  labelledBy?: string
  initialFocusRef?: RefObject<HTMLElement | null>
  dismissible?: boolean
  onClose: () => void
}

interface DialogEntry {
  backdrop: HTMLElement
  card: HTMLElement
}

const dialogs: DialogEntry[] = []
const backgroundState = new Map<HTMLElement, { inert: boolean; hidden: string | null }>()
let savedOverflow: { body: string; root: string } | null = null

function synchronizeBackground(): void {
  const top = dialogs.at(-1)
  if (!top) {
    for (const [element, previous] of backgroundState) {
      element.toggleAttribute('inert', previous.inert)
      if (previous.hidden === null) element.removeAttribute('aria-hidden')
      else element.setAttribute('aria-hidden', previous.hidden)
    }
    backgroundState.clear()
    if (savedOverflow) {
      document.body.style.overflow = savedOverflow.body
      document.documentElement.style.overflow = savedOverflow.root
      savedOverflow = null
    }
    return
  }
  if (!savedOverflow) {
    savedOverflow = { body: document.body.style.overflow, root: document.documentElement.style.overflow }
    document.body.style.overflow = 'hidden'
    document.documentElement.style.overflow = 'hidden'
  }
  for (const element of Array.from(document.body.children)) {
    if (!(element instanceof HTMLElement)) continue
    if (!backgroundState.has(element)) {
      backgroundState.set(element, { inert: element.hasAttribute('inert'), hidden: element.getAttribute('aria-hidden') })
    }
    const inactive = element !== top.backdrop
    element.toggleAttribute('inert', inactive)
    if (inactive) element.setAttribute('aria-hidden', 'true')
    else element.removeAttribute('aria-hidden')
  }
}

function focusableElements(card: HTMLElement): HTMLElement[] {
  return Array.from(card.querySelectorAll<HTMLElement>(
    'button, input:not([type="hidden"]), textarea, select, a[href], [tabindex], [contenteditable="true"]'
  )).filter((element) => {
    if (element.tabIndex < 0 || element.matches(':disabled') || element.closest('[hidden], [inert], [aria-hidden="true"]')) return false
    const style = window.getComputedStyle(element)
    return style.display !== 'none' && style.visibility !== 'hidden'
  })
}

/** Shared modal behavior, including focus containment on WebViews without inert support. */
export function Dialog({ children, className = '', label, labelledBy, initialFocusRef, dismissible = true, onClose }: DialogProps): JSX.Element {
  const backdropRef = useRef<HTMLDivElement>(null)
  const cardRef = useRef<HTMLElement>(null)
  const previousFocusRef = useRef(document.activeElement instanceof HTMLElement ? document.activeElement : null)
  const optionsRef = useRef({ initialFocusRef, dismissible, onClose })
  optionsRef.current = { initialFocusRef, dismissible, onClose }

  useLayoutEffect(() => {
    const backdrop = backdropRef.current
    const card = cardRef.current
    if (!backdrop || !card) return
    const entry = { backdrop, card }
    dialogs.push(entry)
    synchronizeBackground()
    const isTop = (): boolean => dialogs.at(-1) === entry
    const initialFocus = (): void => {
      const preferred = optionsRef.current.initialFocusRef?.current
        ?? card.querySelector<HTMLElement>('[data-dialog-initial-focus]')
        ?? focusableElements(card)[0]
        ?? card
      preferred.focus({ preventScroll: true })
      if (!card.contains(document.activeElement)) card.focus({ preventScroll: true })
    }
    initialFocus()
    const onFocusIn = (event: FocusEvent): void => {
      if (isTop() && !card.contains(event.target as Node)) initialFocus()
    }
    const onKeyDown = (event: KeyboardEvent): void => {
      if (!isTop()) return
      if (event.key === 'Escape') {
        event.preventDefault()
        event.stopPropagation()
        if (optionsRef.current.dismissible) optionsRef.current.onClose()
        return
      }
      if (event.key !== 'Tab') return
      const controls = focusableElements(card)
      const index = controls.indexOf(document.activeElement as HTMLElement)
      if (controls.length === 0) {
        event.preventDefault()
        card.focus()
      } else if (index < 0 || (!event.shiftKey && index === controls.length - 1) || (event.shiftKey && index === 0)) {
        event.preventDefault()
        controls[event.shiftKey ? controls.length - 1 : 0]?.focus()
      }
    }
    document.addEventListener('focusin', onFocusIn, true)
    window.addEventListener('keydown', onKeyDown, true)
    return () => {
      document.removeEventListener('focusin', onFocusIn, true)
      window.removeEventListener('keydown', onKeyDown, true)
      const wasTop = isTop()
      dialogs.splice(dialogs.indexOf(entry), 1)
      synchronizeBackground()
      if (wasTop) {
        const previous = previousFocusRef.current
        if (previous?.isConnected && !previous.closest('[inert]')) previous.focus({ preventScroll: true })
        else dialogs.at(-1)?.card.focus({ preventScroll: true })
      }
    }
  }, [])

  return createPortal(
    <div ref={backdropRef} className="dialog-backdrop" role="presentation" onMouseDown={(event) => {
      if (event.target === event.currentTarget && dismissible) onClose()
    }}>
      <section ref={cardRef} className={`dialog-card ${className}`} role="dialog" aria-modal="true" aria-label={label} aria-labelledby={labelledBy} tabIndex={-1}>
        {children}
      </section>
    </div>,
    document.body
  )
}
