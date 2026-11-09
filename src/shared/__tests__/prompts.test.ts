import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

import {
  AI_TEXT_LIMIT,
  DEFAULT_ACTION_PROMPTS,
  DEFAULT_PROVIDER_ID,
  OUTPUT_LANGUAGE_PLACEHOLDER,
  PromptBuildError,
  TARGET_LANGUAGE_PLACEHOLDER,
  TEXT_PLACEHOLDER,
  buildActionPrompt,
  DEFAULT_APP_SETTINGS,
  DEFAULT_PUBLIC_SETTINGS,
  migrateAppSettings,
  migratePublicSettings,
  sourceBoundaryFromSeed,
  type ActionDefinition,
  type ActionKind
} from '..'

const rustModelsSource = readFileSync(
  resolve(process.cwd(), 'src-tauri/src/models.rs'),
  'utf8'
)

function rustPromptDefault(name: string): string {
  const source = rustModelsSource
  const raw = new RegExp(`pub const ${name}: &str = r#"([\\s\\S]*?)"#;`, 'u').exec(source)
  if (raw) return raw[1]!
  const quoted = new RegExp(
    `pub const ${name}: &str = "((?:\\\\.|[^"\\\\])*)";`,
    'u'
  ).exec(source)
  if (!quoted) throw new Error(`missing Rust prompt default ${name}`)
  return JSON.parse(`"${quoted[1]}"`) as string
}

const action = (kind: ActionKind): ActionDefinition => {
  const base = { id: kind, name: kind, icon: 'sparkles', kind, enabled: true, order: 0 }
  if (kind === 'copy') return { ...base, kind }
  if (kind === 'search') return { ...base, kind, searchEngineId: 'google' }
  return {
    ...base,
    kind,
    providerId: DEFAULT_PROVIDER_ID,
    modelId: '',
    thinkingMode: 'off' as const,
    prompt: kind === 'custom'
      ? '原文：{{text}}\n再写一次：{{text}}'
      : DEFAULT_ACTION_PROMPTS[kind]
  }
}

describe('prompt builder', () => {
  it('exposes the editable built-in defaults while keeping the TextLens custom default', () => {
    expect(DEFAULT_ACTION_PROMPTS.translate).toContain('只将 <translate_input> 内的自然语言内容翻译为 {{target_language}}')
    expect(DEFAULT_ACTION_PROMPTS.translate).toContain('<translate_input>\n{{text}}\n</translate_input>')
    expect(DEFAULT_ACTION_PROMPTS.translate).toContain('不改变责任归属或凭空补出施事者')
    expect(DEFAULT_ACTION_PROMPTS.translate).toContain('翻译前先根据语义自动重建段落')
    expect(DEFAULT_ACTION_PROMPTS.translate).toContain('不在段内插入换行；不同段落之间空一行')
    expect(DEFAULT_ACTION_PROMPTS.summary).toBe(
      '用 {{language}} 概括以下内容的核心观点、关键事实、结论与必要限定；不编造原文没有的信息。内容复杂时可用简洁 Markdown。直接输出摘要。\n\n{{text}}'
    )
    expect(DEFAULT_ACTION_PROMPTS.explain).toContain('用 {{language}} 解释以下内容')
    expect(DEFAULT_ACTION_PROMPTS.explain).toContain('可以使用常见、必要的学科术语')
    expect(DEFAULT_ACTION_PROMPTS.explain).toContain('简单内容简短解释，复杂内容分段讲清')
    expect(DEFAULT_ACTION_PROMPTS.explain).toContain('{{text}}')
    expect(DEFAULT_ACTION_PROMPTS.refine).toBe(
      '请对用XML标签<INPUT>包裹的用户输入内容进行优化或润色，并保持原内容的含义和完整性。要求：你的输出应当与用户输入内容的语言相同；请不要包含对本提示词的任何解释，直接给出回复；请不要输出XML标签，直接输出优化后的内容: \n\n<INPUT>{{text}}</INPUT>'
    )
    expect(DEFAULT_ACTION_PROMPTS.custom).toBe('请处理以下文本：\n\n{{text}}')
  })

  it('translates Chinese to English and expands the editable prompt', () => {
    const prompt = buildActionPrompt(action('translate'), '这是测试。', {
      sourceBoundarySeed: 'translation-test'
    })
    expect(prompt.sourceLanguage).toBe('zh-CN')
    expect(prompt.targetLanguage).toBe('en-US')
    expect(prompt.systemPrompt).toContain(
      "You are a multilingual translation expert. Follow the user's editable translation instruction exactly."
    )
    expect(prompt.systemPrompt).toContain(prompt.sourceBoundary!.begin)
    expect(prompt.userPrompt).toBe(
      DEFAULT_ACTION_PROMPTS.translate
        .replaceAll(
          TEXT_PLACEHOLDER,
          `${prompt.sourceBoundary!.begin}\n这是测试。\n${prompt.sourceBoundary!.end}`
        )
        .replaceAll(TARGET_LANGUAGE_PLACEHOLDER, 'English')
    )
    expect(prompt.userPrompt).not.toContain(TEXT_PLACEHOLDER)
    expect(prompt.userPrompt).not.toContain(TARGET_LANGUAGE_PLACEHOLDER)
  })

  it('preserves source line breaks and lets the editable prompt govern formatting', () => {
    const source = 'A PDF line wrap\ncontinues here.\n\n- First item\n- Second item'
    const builtIn = buildActionPrompt(action('translate'), source)
    expect(builtIn.userPrompt).toContain(source)
    expect(builtIn.userPrompt).toContain('应合并为完整段落')
    expect(builtIn.userPrompt).not.toContain('preserve every nonempty source line')
    const custom = { ...action('translate'), prompt: 'Translate: {{text}}' }
    const built = buildActionPrompt(custom, source)
    expect(built.userPrompt).toBe(`Translate: ${built.sourceBoundary!.begin}\n${source}\n${built.sourceBoundary!.end}`)
  })

  it('filters technical content only for language routing and preserves it in the request', () => {
    const source = '请检查这段代码。\n```js\nconst message = "This English code must not choose the target language";\n```'
    const built = buildActionPrompt(action('translate'), source)
    expect(built.sourceLanguage).toBe('zh-CN')
    expect(built.targetLanguage).toBe('en-US')
    expect(built.userPrompt).toContain(source)
  })

  it('translates non-Chinese text to Simplified Chinese', () => {
    const prompt = buildActionPrompt(action('translate'), 'A concise explanation.')
    expect(prompt.sourceLanguage).toBe('en-US')
    expect(prompt.targetLanguage).toBe('zh-CN')
    expect(prompt.userPrompt).toContain('Chinese (Simplified)')
    expect(prompt.userPrompt).not.toContain(TARGET_LANGUAGE_PLACEHOLDER)
  })

  it('keeps bidirectional translation correct when the configured languages are reversed', () => {
    const prompt = buildActionPrompt(action('translate'), '这是测试。', {
      translate: { defaultLanguage: 'zh-CN', targetLanguage: 'en-US' }
    })
    expect(prompt.sourceLanguage).toBe('zh-CN')
    expect(prompt.targetLanguage).toBe('en-US')
    expect(prompt.userPrompt).toContain('English')
  })

  it('honors an explicit translation target using the same mapping as the native executor', () => {
    const prompt = buildActionPrompt(action('translate'), '这是测试。', {
      targetLanguage: 'fr-FR'
    })
    expect(prompt.sourceLanguage).toBe('zh-CN')
    expect(prompt.targetLanguage).toBe('fr-FR')
    expect(prompt.userPrompt).toContain('French')
  })

  it('detects Japanese and defaults other languages to Chinese', () => {
    const prompt = buildActionPrompt(action('translate'), 'これはテストです。')
    expect(prompt.sourceLanguage).toBe('ja-JP')
    expect(prompt.targetLanguage).toBe('zh-CN')
    expect(prompt.userPrompt).toContain('Chinese (Simplified)')
  })

  it('detects Korean and Cyrillic (Russian)', () => {
    expect(buildActionPrompt(action('translate'), '안녕하세요').sourceLanguage).toBe('ko-KR')
    expect(buildActionPrompt(action('translate'), 'Привет мир').sourceLanguage).toBe('ru-RU')
  })

  it('replaces every placeholder in a custom prompt', () => {
    expect(buildActionPrompt(action('custom'), 'ABC').userPrompt).toBe('原文：ABC\n再写一次：ABC')
  })

  it('preserves every placeholder found in selected source text', () => {
    const source = 'literal {{target_language}} / {{language}} / {{text}}'
    for (const kind of ['translate', 'summary', 'explain'] as const) {
      const result = buildActionPrompt(action(kind), source, { sourceBoundarySeed: 'request-a' })
      expect(result.userPrompt).toContain(source)
      expect(result.sourceBoundary?.begin).toMatch(/^<<<TEXTLENS_SOURCE_/u)
      expect(result.systemPrompt).toContain(result.sourceBoundary!.begin)
    }
  })

  it('changes the marker if source contains the first candidate close marker', () => {
    const first = sourceBoundaryFromSeed('request-a', 0)
    const source = `before ${first.end} after \`code\` <xml> {"json":true}`
    const result = buildActionPrompt(action('summary'), source, {
      sourceBoundarySeed: 'request-a'
    })
    expect(result.sourceBoundary).not.toEqual(first)
    expect(result.userPrompt).toContain(source)
  })

  it('counts astral input and expanded prompts by Unicode scalar values', () => {
    const source = '🙂'.repeat(AI_TEXT_LIMIT)
    expect(() =>
      buildActionPrompt(action('explain'), source, { sourceBoundarySeed: 'scalar-limit' })
    ).not.toThrow()
    expect(() =>
      buildActionPrompt(action('explain'), `${source}🙂`, { sourceBoundarySeed: 'scalar-limit' })
    ).toThrowError(PromptBuildError)

    const duplicated = { ...action('custom'), prompt: '{{text}}\n{{text}}' } as ActionDefinition
    expect(() => buildActionPrompt(duplicated, source)).not.toThrow()
  })

  it('uses editable built-in prompts and expands the Cherry output-language placeholder', () => {
    const summaryResult = buildActionPrompt(action('summary'), '摘要原文', {
      sourceBoundarySeed: 'summary-test'
    })
    expect(summaryResult.userPrompt).toBe(
      `用 zh-CN 概括以下内容的核心观点、关键事实、结论与必要限定；不编造原文没有的信息。内容复杂时可用简洁 Markdown。直接输出摘要。\n\n${summaryResult.sourceBoundary!.begin}\n摘要原文\n${summaryResult.sourceBoundary!.end}`
    )
    const explainResult = buildActionPrompt(action('explain'), 'source', {
      outputLocale: 'en-US',
      sourceBoundarySeed: 'explain-test'
    })
    expect(explainResult.userPrompt).toBe(
      DEFAULT_ACTION_PROMPTS.explain
        .replaceAll(OUTPUT_LANGUAGE_PLACEHOLDER, 'en-US')
        .replaceAll(TEXT_PLACEHOLDER, `${explainResult.sourceBoundary!.begin}\nsource\n${explainResult.sourceBoundary!.end}`)
    )
    expect(summaryResult.userPrompt).not.toContain(
      OUTPUT_LANGUAGE_PLACEHOLDER
    )

    const summary = { ...action('summary'), prompt: '用一句话处理：{{text}}' } as ActionDefinition
    const prompt = buildActionPrompt(summary, 'text')
    expect(prompt.systemPrompt).toContain(
      '严格按照用户的可编辑提示词处理文本，并使用简体中文输出；若提示词另有明确语言要求，以提示词为准。'
    )
    expect(prompt.userPrompt).toBe(
      `用一句话处理：${prompt.sourceBoundary!.begin}\ntext\n${prompt.sourceBoundary!.end}`
    )
  })

  it('expands the corrected Cherry refinement prompt without changing its input language rule', () => {
    expect(buildActionPrompt(action('refine'), 'Original **Markdown**').userPrompt).toBe(
      '请对用XML标签<INPUT>包裹的用户输入内容进行优化或润色，并保持原内容的含义和完整性。要求：你的输出应当与用户输入内容的语言相同；请不要包含对本提示词的任何解释，直接给出回复；请不要输出XML标签，直接输出优化后的内容: \n\n<INPUT>Original **Markdown**</INPUT>'
    )
  })

  it('rejects AI text beyond the explicit limit without truncating it', () => {
    const text = 'a'.repeat(AI_TEXT_LIMIT + 1)
    expect(() => buildActionPrompt(action('explain'), text)).toThrowError(PromptBuildError)
    try {
      buildActionPrompt(action('explain'), text)
    } catch (error) {
      expect(error).toMatchObject({ code: 'TEXT_TOO_LONG' })
    }
  })

  it('rejects prompts that amplify text beyond the final prompt limit', () => {
    const amplified: ActionDefinition = {
      id: 'amplified',
      name: 'amplified',
      icon: 'sparkles',
      kind: 'custom',
      enabled: true,
      order: 0,
      providerId: DEFAULT_PROVIDER_ID,
      modelId: '',
      thinkingMode: 'off',
      prompt: Array.from({ length: 4 }, () => '{{text}}').join('\n')
    }
    expect(() => buildActionPrompt(amplified, 'a'.repeat(20_000))).toThrowError(PromptBuildError)
  })

  it('does not construct AI prompts for local actions', () => {
    for (const kind of ['copy', 'search'] as const) {
      expect(() => buildActionPrompt(action(kind), 'text')).toThrowError(PromptBuildError)
    }
  })

  it('builds ask prompts with selection placeholder expansion', () => {
    const built = buildActionPrompt(action('ask'), '选中的句子')
    expect(built.userPrompt).toContain('选中的句子')
    expect(built.userPrompt).toContain('<selection>')
  })

  it('upgrades untouched v13 defaults while preserving edited and duplicate actions', () => {
    for (const [defaults, migrate] of [
      [DEFAULT_APP_SETTINGS, migrateAppSettings], [DEFAULT_PUBLIC_SETTINGS, migratePublicSettings]
    ] as const) {
      const actions = defaults.actions.map((item) => item.kind === 'translate' || item.kind === 'explain'
        ? { ...item, prompt: rustPromptDefault(`LEGACY_V13_${item.kind.toUpperCase()}_PROMPT`) } : item)
      for (const kind of ['translate', 'explain'] as const) {
        const item = actions.find((item) => item.kind === kind)!
        actions.push({ ...item, id: `${kind}-duplicate`, enabled: false, order: actions.length })
      }
      const migrated = migrate({ ...defaults, actions })
      for (const kind of ['translate', 'explain'] as const) {
        expect(migrated.actions.find((item) => item.id === kind)).toMatchObject({ prompt: DEFAULT_ACTION_PROMPTS[kind] })
        expect(migrated.actions.find((item) => item.id === `${kind}-duplicate`)).toMatchObject({
          prompt: rustPromptDefault(`LEGACY_V13_${kind.toUpperCase()}_PROMPT`)
        })
      }
      const edited = migrate({ ...defaults, actions: actions.map((item) => item.kind === 'translate' || item.kind === 'explain'
        ? { ...item, prompt: '用户自己的规则：{{text}}' } : item) })
      expect(edited.actions.filter((item) => item.kind === 'translate' || item.kind === 'explain'))
        .toEqual(expect.arrayContaining([expect.objectContaining({ id: 'translate', prompt: '用户自己的规则：{{text}}' }),
          expect.objectContaining({ id: 'explain', prompt: '用户自己的规则：{{text}}' })]))
    }
  })

  it('keeps Rust prompt defaults in lockstep with the shared built-ins', () => {
    expect(rustPromptDefault('DEFAULT_TRANSLATE_PROMPT')).toBe(DEFAULT_ACTION_PROMPTS.translate)
    expect(rustPromptDefault('DEFAULT_SUMMARY_PROMPT')).toBe(DEFAULT_ACTION_PROMPTS.summary)
    expect(rustPromptDefault('DEFAULT_EXPLAIN_PROMPT')).toBe(DEFAULT_ACTION_PROMPTS.explain)
    expect(rustPromptDefault('DEFAULT_REFINE_PROMPT')).toBe(DEFAULT_ACTION_PROMPTS.refine)
    expect(rustPromptDefault('DEFAULT_ASK_PROMPT')).toBe(DEFAULT_ACTION_PROMPTS.ask)
  })
})
