import { createElement } from 'react'
import { fireEvent, render, screen } from '@testing-library/react'

import { DEFAULT_ACTION_PROMPTS, type ActionDefinition } from '../../shared'
import { CustomActionDialog, promptAfterKindChange } from './CustomActionDialog'

describe('promptAfterKindChange', () => {
  it('uses the matching default prompt when a new action changes AI type', () => {
    expect(
      promptAfterKindChange('custom', 'translate', DEFAULT_ACTION_PROMPTS.custom)
    ).toBe(DEFAULT_ACTION_PROMPTS.translate)
    expect(
      promptAfterKindChange('translate', 'summary', DEFAULT_ACTION_PROMPTS.translate)
    ).toBe(DEFAULT_ACTION_PROMPTS.summary)
    expect(
      promptAfterKindChange('summary', 'explain', DEFAULT_ACTION_PROMPTS.summary)
    ).toBe(DEFAULT_ACTION_PROMPTS.explain)
  })

  it('does not overwrite a prompt the user has customized', () => {
    const customized = '请翻译成日语：{{text}}'
    expect(promptAfterKindChange('translate', 'summary', customized)).toBe(customized)
    expect(promptAfterKindChange('custom', 'copy', customized)).toBe(customized)
  })

  it('restores the current action type default without saving immediately', () => {
    const action: ActionDefinition = {
      id: 'translate-second',
      name: '第二个翻译',
      icon: 'languages',
      kind: 'translate',
      enabled: false,
      order: 8,
      prompt: '用户修改后的翻译提示词：{{text}}',
      providerId: 'provider-one',
      modelId: 'model-one',
      thinkingMode: 'off'
    }
    const onSave = vi.fn()
    render(createElement(CustomActionDialog, {
      action,
      providers: [{
        id: 'provider-one',
        name: '服务商一',
        enabled: true,
        baseUrl: 'https://example.com/v1',
        keyConfigured: true,
        models: [{ id: 'model-one', name: '模型一', thinkingLevels: [] }]
      }],
      onCancel: vi.fn(),
      onSave
    }))

    const textarea = document.getElementById('action-prompt') as HTMLTextAreaElement
    expect(textarea).toHaveValue(action.prompt)
    fireEvent.click(screen.getByRole('button', { name: '恢复默认提示词' }))
    expect(textarea).toHaveValue(DEFAULT_ACTION_PROMPTS.translate)
    expect(onSave).not.toHaveBeenCalled()

    fireEvent.click(screen.getByRole('button', { name: '保存修改' }))
    expect(onSave).toHaveBeenCalledWith(expect.objectContaining({
      kind: 'translate',
      prompt: DEFAULT_ACTION_PROMPTS.translate
    }))
  })
})

describe('CustomActionDialog layout', () => {
  const action: ActionDefinition = {
    id: 'copy-second',
    name: '复制',
    icon: 'clipboard',
    kind: 'copy',
    enabled: false,
    order: 7
  }

  it('offers the action type only when adding a new action', () => {
    const { unmount } = render(createElement(CustomActionDialog, {
      action,
      providers: [],
      onCancel: vi.fn(),
      onSave: vi.fn()
    }))
    expect(screen.queryByText('动作类型')).not.toBeInTheDocument()
    expect(document.querySelector('.dialog-form-grid')).toHaveClass('dialog-form-grid--single')
    unmount()

    render(createElement(CustomActionDialog, {
      action: null,
      providers: [],
      onCancel: vi.fn(),
      onSave: vi.fn()
    }))
    expect(screen.getByText('动作类型')).toBeInTheDocument()
    expect(document.querySelector('.dialog-form-grid')).not.toHaveClass('dialog-form-grid--single')
  })

  it('locks the background scroll while open and restores it on close', () => {
    document.body.style.overflow = 'scroll'
    document.documentElement.style.overflow = 'scroll'
    const { unmount } = render(createElement(CustomActionDialog, {
      action,
      providers: [],
      onCancel: vi.fn(),
      onSave: vi.fn()
    }))
    expect(document.body.style.overflow).toBe('hidden')
    expect(document.documentElement.style.overflow).toBe('hidden')

    unmount()
    expect(document.body.style.overflow).toBe('scroll')
    expect(document.documentElement.style.overflow).toBe('scroll')
    document.body.style.overflow = ''
    document.documentElement.style.overflow = ''
  })
})

describe('CustomActionDialog model + thinking', () => {
  const modelRoute = (providerId: string, modelId: string): string =>
    JSON.stringify([providerId, modelId])

  it('uses a unified provider+model list and saves thinking mode', () => {
    const onSave = vi.fn()
    render(createElement(CustomActionDialog, {
      action: null,
      providers: [{
        id: 'provider-one',
        name: '服务商一',
        enabled: true,
        baseUrl: 'https://example.com/v1',
        keyConfigured: true,
        models: [{
          id: 'model-think',
          name: '思考模型',
          thinkingLevels: ['low', 'medium', 'high']
        }]
      }],
      onCancel: vi.fn(),
      onSave
    }))

    fireEvent.change(screen.getByLabelText('动作名称'), { target: { value: '深度思考' } })
    fireEvent.change(screen.getByLabelText('模型'), {
      target: { value: modelRoute('provider-one', 'model-think') }
    })

    const thinkingSelect = screen.getByLabelText('思考强度')
    expect(thinkingSelect).toBeInTheDocument()
    expect(screen.getByRole('option', { name: '关闭思考（更快首字）' })).toBeInTheDocument()
    expect(thinkingSelect).toHaveValue('off')

    fireEvent.change(thinkingSelect, { target: { value: 'medium' } })
    fireEvent.click(screen.getByRole('button', { name: '添加动作' }))

    expect(onSave).toHaveBeenCalledWith(expect.objectContaining({
      kind: 'custom',
      providerId: 'provider-one',
      modelId: 'model-think',
      thinkingMode: 'medium'
    }))
  })

  it('lets a non-disableable model select off and saves the choice', () => {
    const onSave = vi.fn()
    render(createElement(CustomActionDialog, {
      action: null,
      providers: [{ id: 'provider-one', name: '服务商一', enabled: true, baseUrl: 'https://example.com/v1', keyConfigured: true,
        models: [{ id: 'gpt-6-astra', name: 'Astra', thinkingLevels: ['low', 'medium'], thinkingCapability: { source: 'explicit', dialect: 'reasoningEffort', supportsOff: false } }] }],
      onCancel: vi.fn(), onSave
    }))
    fireEvent.change(screen.getByLabelText('动作名称'), { target: { value: '测试' } })
    fireEvent.change(screen.getByLabelText('模型'), { target: { value: modelRoute('provider-one', 'gpt-6-astra') } })
    const select = screen.getByLabelText('思考强度')
    expect(screen.getByRole('option', { name: '关闭思考（使用最低支持档位）' })).toBeInTheDocument()
    fireEvent.change(select, { target: { value: 'off' } })
    fireEvent.click(screen.getByRole('button', { name: '添加动作' }))
    expect(onSave).toHaveBeenCalledWith(expect.objectContaining({ thinkingMode: 'off' }))
  })

  it('hides disabled providers and leaves reasoner-only models without thinking controls', () => {
    render(createElement(CustomActionDialog, {
      action: null,
      providers: [
        {
          id: 'provider-off',
          name: '已关闭',
          enabled: false,
          baseUrl: 'https://example.com/v1',
          keyConfigured: true,
          models: [{ id: 'hidden-model', name: '隐藏模型', thinkingLevels: [] }]
        },
        {
          id: 'provider-on',
          name: '启用',
          enabled: true,
          baseUrl: 'https://example.com/v1',
          keyConfigured: true,
          models: [{ id: 'deepseek-r1', name: 'DeepSeek R1', thinkingLevels: [] }]
        }
      ],
      onCancel: vi.fn(),
      onSave: vi.fn()
    }))

    const modelSelect = screen.getByLabelText('模型') as HTMLSelectElement
    const optionTexts = Array.from(modelSelect.querySelectorAll('option')).map(
      (option) => option.textContent
    )
    expect(optionTexts.join('\n')).not.toContain('隐藏模型')
    expect(optionTexts.join('\n')).toContain('DeepSeek R1')

    fireEvent.change(modelSelect, {
      target: { value: modelRoute('provider-on', 'deepseek-r1') }
    })
    expect(screen.queryByLabelText('思考强度')).not.toBeInTheDocument()
  })

  it('hides thinking select when model has no thinking levels', () => {
    render(createElement(CustomActionDialog, {
      action: null,
      providers: [{
        id: 'provider-one',
        name: '服务商一',
        enabled: true,
        baseUrl: 'https://example.com/v1',
        keyConfigured: true,
        models: [{ id: 'model-plain', name: '普通模型', thinkingLevels: [] }]
      }],
      onCancel: vi.fn(),
      onSave: vi.fn()
    }))

    fireEvent.change(screen.getByLabelText('模型'), {
      target: { value: modelRoute('provider-one', 'model-plain') }
    })
    expect(screen.queryByLabelText('思考强度')).not.toBeInTheDocument()
  })
})


describe('CustomActionDialog thinking detection', () => {
  const providers = [{
    id: 'provider-one', name: '服务商一', enabled: true,
    baseUrl: 'https://example.com/v1', keyConfigured: true,
    models: [{ id: 'gpt-5.2', name: 'GPT 5.2', thinkingLevels: ['low' as const],
      thinkingEnabled: false,
      thinkingCapability: { source: 'manual' as const, dialect: 'reasoningEffort' as const, supportsOff: true } }]
  }]
  const action: ActionDefinition = {
    id: 'tool', name: '工具', icon: 'sparkles', kind: 'custom', enabled: true, order: 1,
    providerId: 'provider-one', modelId: 'gpt-5.2', thinkingMode: 'low', prompt: '处理：{{text}}'
  }

  it('re-detects in the tool editor and saves only its chosen effort', () => {
    const onSave = vi.fn()
    render(createElement(CustomActionDialog, { action, providers, onSave, onCancel: vi.fn() }))
    expect(screen.getByRole('switch', { name: '启用思考' })).toBeChecked()
    expect(screen.queryByRole('option', { name: '极高' })).not.toBeInTheDocument()
    fireEvent.click(screen.getByRole('button', { name: '重新自动识别' }))
    expect(screen.getByRole('option', { name: '极高' })).toBeInTheDocument()
    fireEvent.change(screen.getByLabelText('思考强度'), { target: { value: 'xhigh' } })
    fireEvent.click(screen.getByRole('button', { name: '保存修改' }))
    expect(onSave).toHaveBeenCalledWith(expect.objectContaining({
      thinkingMode: 'xhigh',
      thinkingModel: expect.objectContaining({ thinkingLevels: ['low', 'medium', 'high', 'xhigh'],
        thinkingCapability: { source: 'heuristic', dialect: 'reasoningEffort', supportsOff: true } })
    }))
    expect(providers[0]!.models[0]!.thinkingLevels).toEqual(['low'])
  })

  it('keeps re-detection local until saved and provides a working off switch', () => {
    const onSave = vi.fn()
    const onCancel = vi.fn()
    render(createElement(CustomActionDialog, { action, providers, onSave, onCancel }))
    fireEvent.click(screen.getByRole('button', { name: '重新自动识别' }))
    fireEvent.click(screen.getByRole('switch', { name: '启用思考' }))
    expect(screen.getByLabelText('思考强度')).toHaveValue('off')
    fireEvent.click(screen.getByRole('button', { name: '取消' }))
    expect(onCancel).toHaveBeenCalledOnce()
    expect(onSave).not.toHaveBeenCalled()
    expect(providers[0]!.models[0]!.thinkingLevels).toEqual(['low'])
  })

  it('discards obsolete heuristic levels when the current protocol cannot detect the model', () => {
    render(createElement(CustomActionDialog, { action, providers: [{
      ...providers[0]!, protocol: 'gemini', models: [{ ...providers[0]!.models[0]!,
        thinkingLevels: ['low', 'medium', 'high'],
        thinkingCapability: { source: 'heuristic', dialect: 'reasoningEffort', supportsOff: true }
      }]
    }], onSave: vi.fn(), onCancel: vi.fn() }))
    expect(screen.queryByLabelText('思考强度')).not.toBeInTheDocument()
    expect(screen.queryByRole('switch', { name: '启用思考' })).not.toBeInTheDocument()
  })

  it('respects explicit empty capability evidence until re-detection', () => {
    render(createElement(CustomActionDialog, { action, providers: [{ ...providers[0]!, models: [{
      ...providers[0]!.models[0]!, thinkingLevels: [],
      thinkingCapability: { source: 'explicit', dialect: null, supportsOff: false }
    }] }], onSave: vi.fn(), onCancel: vi.fn() }))
    expect(screen.queryByLabelText('思考强度')).not.toBeInTheDocument()
    fireEvent.click(screen.getByRole('button', { name: '重新自动识别' }))
    expect(screen.getByLabelText('思考强度')).toBeInTheDocument()
  })
})
