/**
 * Validates an output-boundary id (= the instance port name it exposes).
 * Mirror of the reserved list in src/graph/validation.rs::RESERVED_OUTPUT_IDS.
 */
const RESERVED = ['input', 'error', 'in', 'out', 'success'];

export function validatePortName(
  name: string,
  takenIds: string[],
  selfId?: string
): string | null {
  if (!name.trim()) return 'A port name is required';
  if (RESERVED.includes(name)) return `'${name}' is a reserved port name`;
  if (name.includes('/')) return "Port names must not contain '/'";
  if (takenIds.some((id) => id === name && id !== selfId))
    return `A node named '${name}' already exists`;
  return null;
}
