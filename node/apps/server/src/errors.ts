import { Conflict, type ErrorCode, InvalidEdit, toTextdbError } from '@textdb/node';
import type { Context } from 'hono';

// TX005 is 403 and TX003 is 404, and that distinction is the whole point of having both: a path
// outside every share is not there, one inside a share you may not write is refused.
const STATUS = { TX000: 500, TX001: 409, TX002: 503, TX003: 404, TX004: 400, TX005: 403 } as const satisfies Record<ErrorCode, number>;

export interface ErrorBody {
  code: ErrorCode;
  message: string;
  conflict?: Record<string, unknown>;
}

/** An error in the store's terms raised by the server itself, e.g. one the textdb CLI reported. */
export class CodedError extends Error {
  readonly code: ErrorCode;
  /** The status to answer with, where the code's own is not the right one. */
  readonly status: number | undefined;

  constructor(code: ErrorCode, message: string, status?: number) {
    super(message);
    this.code = code;
    this.status = status;
  }
}

/**
 * No bearer, where this server takes none without one: 401, not 403.
 *
 * The difference is worth keeping: 401 says "say who you are", 403 says "you did, and no". The
 * store's own codes have no word for the first, because the store is never asked without one.
 */
export function unauthorized(message: string): CodedError {
  return new CodedError('TX005', message, 401);
}

export function errorResponse(c: Context, error: unknown): Response {
  if (error instanceof CodedError) {
    return c.json({ code: error.code, message: error.message }, (error.status ?? STATUS[error.code]) as 400);
  }
  const err = toTextdbError(error);
  const body: ErrorBody = { code: err.code, message: err.message };
  if (err instanceof Conflict) body.conflict = err.payload;
  if (err.code === 'TX000') console.error(error);
  return c.json(body, STATUS[err.code]);
}

/** Malformed requests are reported as TX004 so every error keeps the same shape. */
export function badRequest(message: string): InvalidEdit {
  return new InvalidEdit(message);
}
