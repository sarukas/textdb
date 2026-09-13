import { Conflict, type ErrorCode, InvalidEdit, toTextdbError } from '@textdb/node';
import type { Context } from 'hono';

const STATUS = { TX000: 500, TX001: 409, TX002: 503, TX003: 404, TX004: 400 } as const satisfies Record<ErrorCode, number>;

export interface ErrorBody {
  code: ErrorCode;
  message: string;
  conflict?: Record<string, unknown>;
}

export function errorResponse(c: Context, error: unknown): Response {
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
