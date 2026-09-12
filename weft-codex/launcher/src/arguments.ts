export function hostCommand(argv: readonly string[]): string {
  const firstArgument = argv[2]
  if (firstArgument === "--version" || firstArgument === "--help") return firstArgument
  if (!firstArgument || firstArgument.startsWith("--")) return "start"
  return firstArgument
}
