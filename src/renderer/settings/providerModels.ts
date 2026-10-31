import type { ProviderModel } from '../../shared'

function toCheckedSet(checkedIds: ReadonlySet<string> | string[]): ReadonlySet<string> {
  return checkedIds instanceof Set ? checkedIds : new Set(checkedIds)
}

function mergeThinkingSettings(previous: ProviderModel, remote: ProviderModel): ProviderModel {
  const manual = previous.thinkingCapability?.source === 'manual'
  if (manual) {
    return {
      ...remote,
      thinkingEnabled: previous.thinkingEnabled,
      thinkingLevels: previous.thinkingLevels,
      thinkingCapability: previous.thinkingCapability,
      thinkingDefault: previous.thinkingDefault
    }
  }
  return {
    ...remote,
    thinkingEnabled: previous.thinkingEnabled,
    thinkingDefault:
      previous.thinkingDefault && remote.thinkingLevels.includes(previous.thinkingDefault)
        ? previous.thinkingDefault
        : remote.thinkingDefault
  }
}

/**
 * Merge previous curated models with a remote catalog under a multi-select pick.
 *
 * Rules:
 * 1. Keep previous models that remain checked, in previous order.
 * 2. Append newly checked remote models (not already kept) in remote list order.
 * 3. For ids present in remote, use remote name / thinking detection, but keep
 *    user-edited thinking levels and the per-model enable toggle.
 * 4. Previous models not in remote but still checked (manual) stay with previous metadata.
 * 5. Unchecked previous models drop out.
 * 6. No duplicate ids.
 */
export function mergeProviderModelsOnPick(args: {
  previous: ProviderModel[]
  remote: ProviderModel[]
  checkedIds: ReadonlySet<string> | string[]
}): ProviderModel[] {
  const checked = toCheckedSet(args.checkedIds)
  if (checked.size === 0) return []

  const remoteById = new Map<string, ProviderModel>()
  for (const model of args.remote) {
    if (!remoteById.has(model.id)) remoteById.set(model.id, model)
  }

  const result: ProviderModel[] = []
  const seen = new Set<string>()

  for (const prev of args.previous) {
    if (!checked.has(prev.id) || seen.has(prev.id)) continue
    const remote = remoteById.get(prev.id)
    result.push(remote ? mergeThinkingSettings(prev, remote) : { ...prev })
    seen.add(prev.id)
  }

  for (const remote of args.remote) {
    if (!checked.has(remote.id) || seen.has(remote.id)) continue
    result.push({ ...remote })
    seen.add(remote.id)
  }

  return result
}
