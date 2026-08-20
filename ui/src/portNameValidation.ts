/**
 * Validates a boundary id (= the instance port name it exposes), per kind.
 * Mirrors src/graph/validation.rs::RESERVED_OUTPUT_IDS / RESERVED_ERROR_IDS:
 * each kind's special id (`output` -> success, `error` -> default error) is
 * legal for its own kind and reserved for the other.
 */
const RESERVED: Record<'output' | 'error', string[]> = {
  output: ['input', 'error', 'in', 'out', 'success'],
  error: ['input', 'output', 'in', 'out', 'success'],
};

export function validatePortName(
  name: string,
  takenIds: string[],
  kind: 'output' | 'error',
  selfId?: string
): string | null {
  if (!name.trim()) return 'A port name is required';
  if (RESERVED[kind].includes(name)) return `'${name}' is a reserved port name`;
  if (name.includes('/')) return "Port names must not contain '/'";
  if (takenIds.some((id) => id === name && id !== selfId))
    return `A node named '${name}' already exists`;
  return null;
}
