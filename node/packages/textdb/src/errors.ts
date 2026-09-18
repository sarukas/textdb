export type ErrorCode = 'TX000' | 'TX001' | 'TX002' | 'TX003' | 'TX004' | 'TX005';

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

/**
 * It is in your view and you may not do it: a read-only share written to, a folder never granted
 * (under an alias you hold), an owner-only operation on a token session.
 *
 * Not the same as `NotFound`, which is what a path outside every share answers -- the difference is
 * the point of it, so the two must not be collapsed by a caller either.
 */
export class Forbidden extends TextdbError {
  constructor(message: string, payload: Record<string, unknown> = {}) {
    super(message, 'TX005', payload);
    this.name = 'Forbidden';
  }
}

/**
 * The store cannot do this at all, on this engine.
 *
 * Not a refusal and not a bad request: the trash and the one-batch undo read shadow tables that
 * only the SQLite schema has, so against Postgres there is nothing to ask. `CorpusApi.capabilities`
 * says which, so a client can avoid offering it rather than meeting this.
 *
 * Carries `TX004`, because on the wire it is a request the store will not take.
 */
export class Unsupported extends TextdbError {
  constructor(message: string) {
    super(message, 'TX004');
    this.name = 'Unsupported';
  }
}

export class InvalidEdit extends TextdbError {
  constructor(message: string, payload: Record<string, unknown> = {}) {
    super(message, 'TX004', payload);
  }
}

const CODES = ['TX001', 'TX002', 'TX003', 'TX004', 'TX005', 'TX000'] as const;

/** The error class a code means: `TX005` is a `Forbidden`, `TX003` a `NotFound`, and so on. */
export function errorOf(code: ErrorCode, message: string, payload: Record<string, unknown> = {}): TextdbError {
  switch (code) {
    case 'TX001':
      return new Conflict(message, payload);
    case 'TX002':
      return new Contention(message, payload);
    case 'TX003':
      return new NotFound(message, payload);
    case 'TX004':
      return new InvalidEdit(message, payload);
    case 'TX005':
      return new Forbidden(message, payload);
    default:
      return new TextdbError(message, code, payload);
  }
}

function fromCode(code: ErrorCode, message: string, detail?: string): TextdbError {
  let payload: Record<string, unknown> = {};
  if (detail) {
    try {
      payload = JSON.parse(detail) as Record<string, unknown>;
    } catch {
      payload = { detail };
    }
  }
  return errorOf(code, message, payload);
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
