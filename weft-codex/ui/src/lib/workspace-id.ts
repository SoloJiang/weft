/** Choose which workspace to show, in product order. */
export function pickWorkspaceId(
  ids: number[],
  choices: {
    preferredId?: number | null
    currentId?: number | null
    persistedId?: number | null
  },
): number | null {
  const known = new Set(ids)
  if (choices.preferredId && known.has(choices.preferredId)) return choices.preferredId
  if (choices.currentId && known.has(choices.currentId)) return choices.currentId
  if (choices.persistedId && known.has(choices.persistedId)) return choices.persistedId
  return ids[0] ?? null
}
