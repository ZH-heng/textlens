import { StrictMode, useState } from 'react'
import { fireEvent, render, screen } from '@testing-library/react'
import { Dialog } from './Dialog'

function Harness(): React.JSX.Element {
  const [open, setOpen] = useState(false)
  const [nested, setNested] = useState(false)
  return <>
    <button onClick={() => setOpen(true)}>打开</button>
    {open && <Dialog label="编辑" onClose={() => setOpen(false)}>
      <input aria-label="内容" data-dialog-initial-focus />
      <button onClick={() => setNested(true)}>更多</button>
      <button onClick={() => setOpen(false)}>关闭</button>
      {nested && <Dialog label="确认" onClose={() => setNested(false)}>
        <button data-dialog-initial-focus onClick={() => setNested(false)}>取消确认</button>
      </Dialog>}
    </Dialog>}
  </>
}

describe('shared Dialog', () => {
  it('focuses, traps Tab in both directions, blocks background focus and restores its trigger', () => {
    const view = render(<StrictMode><Harness /></StrictMode>)
    const trigger = screen.getByRole('button', { name: '打开' })
    trigger.focus()
    fireEvent.click(trigger)
    const first = screen.getByRole('textbox', { name: '内容' })
    const last = screen.getByRole('button', { name: '关闭' })
    expect(first).toHaveFocus()
    expect(view.container).toHaveAttribute('inert')
    fireEvent.keyDown(first, { key: 'Tab', shiftKey: true })
    expect(last).toHaveFocus()
    fireEvent.keyDown(last, { key: 'Tab' })
    expect(first).toHaveFocus()
    trigger.focus()
    expect(first).toHaveFocus()
    fireEvent.keyDown(window, { key: 'Escape' })
    expect(trigger).toHaveFocus()
    expect(view.container).not.toHaveAttribute('inert')
    expect(view.container).not.toHaveAttribute('aria-hidden')
  })

  it('keeps a nested dialog on top and restores the parent dialog before the page', () => {
    render(<Harness />)
    const trigger = screen.getByRole('button', { name: '打开' })
    trigger.focus()
    fireEvent.click(trigger)
    const more = screen.getByRole('button', { name: '更多' })
    more.focus()
    fireEvent.click(more)
    expect(screen.getByRole('button', { name: '取消确认' })).toHaveFocus()
    expect(screen.queryByRole('dialog', { name: '编辑' })).not.toBeInTheDocument()
    fireEvent.keyDown(window, { key: 'Escape' })
    expect(screen.getByRole('dialog', { name: '编辑' })).toBeInTheDocument()
    expect(more).toHaveFocus()
    fireEvent.keyDown(window, { key: 'Escape' })
    expect(trigger).toHaveFocus()
  })

  it('does not reset focus when its callbacks change after a parent render', () => {
    const onClose = vi.fn()
    const view = render(<Dialog label="编辑" onClose={onClose}><input aria-label="字段" /><button>保存</button></Dialog>)
    const save = screen.getByRole('button', { name: '保存' })
    save.focus()
    const updated = vi.fn()
    view.rerender(<Dialog label="编辑" onClose={updated}><input aria-label="字段" /><button>保存</button></Dialog>)
    expect(save).toHaveFocus()
    fireEvent.keyDown(window, { key: 'Escape' })
    expect(updated).toHaveBeenCalledOnce()
    expect(onClose).not.toHaveBeenCalled()
  })

  it('focuses the card and blocks dismissal while every control is disabled', () => {
    const onClose = vi.fn()
    render(<Dialog label="等待" dismissible={false} onClose={onClose}><button disabled data-dialog-initial-focus>保存中</button></Dialog>)
    expect(screen.getByRole('dialog', { name: '等待' })).toHaveFocus()
    fireEvent.keyDown(window, { key: 'Tab' })
    fireEvent.keyDown(window, { key: 'Escape' })
    expect(onClose).not.toHaveBeenCalled()
    expect(screen.getByRole('dialog', { name: '等待' })).toHaveFocus()
  })
})
