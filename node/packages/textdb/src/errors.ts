export type ErrorCode = 'TX000' | 'TX001' | 'TX002' | 'TX003' | 'TX004';

export class TextdbError extends Error {
  readonly code: ErrorCode;
  readonly payload: Record<string, unknown>;

  constructor(message: string, code: ErrorCode = 'TX000', payload: Record<string, unknown> = {}) {
    super(message);
    this.name = new.target.name;
    this.code = code;
    this.payload = payload;
  }
}

/** The detail of a TX001. `theirs` is the *current* text of the region; retry with `current_version`. */
export interface ConflictPayload {
  path: string;
  region_line_from: number;
  region_line_to: number;
  base: string;
  theirs: string;
  ours: string;
  current_version: number;
}

export class Conflict extends TextdbError {
  declare readonly payload: ConflictPayload & Record<string, unknown>;

  constructor(message: string, payload: Record<string, unknown> = {}) {
    super(message, 'TX001', payload);
  }
}

export class Contention extends TextdbError {
  constructor(message: string, payload: Record<string, unknown> = {}) {
    super(message, 'TX002', payload);
  }
}

export class NotFound extends TextdbError {
  constructor(message: string, payload: Record<string, unknown> = {}) {
    super(message, 'TX003', payload);
  }
}

export class InvalidEdit extends TextdbError {
  constructor(message: string, payload: Record<string, unknown> = {}) {
    super(message, 'TX004', payload);
  }
}

const CODES = ['TX001', 'TX002', 'TX003', 'TX004', 'TX000'] as const;

function fromCode(code: ErrorCode, message: string, detail?: string): TextdbError {
  let payload: Record<string, unknown> = {};
  if (detail) {
    try {
      payload = JSON.parse(detail) as Record<string, unknown>;
    } catch {
      payload = { detail };
    }
  }
  switch (code) {
    case 'TX001':
      return new Conflict(message, payload);
    case 'TX002':
      return new Contention(message, payload);
    case 'TX003':
      return new NotFound(message, payload);
    case 'TX004':
      return new InvalidEdit(message, payload);
    default:
      return new TextdbError(message, code, payload);
  }
}

/** Parse the SQLite form, `TX001 conflict: {json}` / `TX004 invalid edit: …` (as python/textdb/errors.py). */
export function fromMessage(message: string): TextdbError | undefined {
  for (const code of CODES) {
    const idx = message.indexOf(code);
    if (idx < 0) continue;
    let rest = message.slice(idx + code.length).trim();
    let detail: string | undefined;
    const brace = rest.indexOf('{');
    if (code === 'TX001' && brace >= 0) {
      detail = rest.slice(brace);
      rest = rest.slice(0, brace).replace(/[: ]+$/, '');
    }
    return fromCode(code, rest || message, detail);
  }
  return undefined;
}

export function toTextdbError(error: unknown): TextdbError {
  if (error instanceof TextdbError) return error;
  const message = error instanceof Error ? error.message : String(error);
  return fromMessage(message) ?? new TextdbError(message);
}
