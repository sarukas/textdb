import type { Context } from 'hono';
import { badRequest } from './errors.ts';

export function queryString(c: Context, name: string): string {
  const value = c.req.query(name);
  if (value === undefined || value === '') throw badRequest(`missing query parameter: ${name}`);
  return value;
}

export function queryInt(c: Context, name: string): number | undefined {
  const value = c.req.query(name);
  return value === undefined || value === '' ? undefined : parseInteger(value, name);
}

export function parseInteger(value: string, name: string): number {
  if (!/^-?\d+$/.test(value.trim())) throw badRequest(`${name} must be an integer`);
  return Number(value);
}

export async function jsonBody(c: Context): Promise<Record<string, unknown>> {
  let body: unknown;
  try {
    body = await c.req.json();
  } catch {
    throw badRequest('request body must be JSON');
  }
  if (typeof body !== 'object' || body === null || Array.isArray(body)) {
    throw badRequest('request body must be a JSON object');
  }
  return body as Record<string, unknown>;
}

export function bodyString(body: Record<string, unknown>, name: string): string {
  const value = body[name];
  if (typeof value !== 'string') throw badRequest(`${name} must be a string`);
  return value;
}

export function bodyOptionalString(body: Record<string, unknown>, name: string): string | undefined {
  const value = body[name];
  if (value === undefined || value === null) return undefined;
  if (typeof value !== 'string') throw badRequest(`${name} must be a string`);
  return value;
}

export function bodyInt(body: Record<string, unknown>, name: string): number {
  const value = body[name];
  if (!Number.isInteger(value)) throw badRequest(`${name} must be an integer`);
  return value as number;
}

/** Files in one import request; the web UI sends a few megabytes at a time. */
const MAX_IMPORT_FILES = 5000;

export function bodyImportFiles(body: Record<string, unknown>): { path: string; content: string }[] {
  const files = body.files;
  if (!Array.isArray(files) || files.length === 0) throw badRequest('files must be a non-empty array');
  if (files.length > MAX_IMPORT_FILES) throw badRequest(`at most ${MAX_IMPORT_FILES} files per request`);
  return files.map((file: unknown, i) => {
    const f = (typeof file === 'object' && file !== null ? file : {}) as Record<string, unknown>;
    if (typeof f.path !== 'string' || typeof f.content !== 'string') {
      throw badRequest(`files[${i}] must be an object with string path and content`);
    }
    return { path: f.path, content: f.content };
  });
}

export function bodyOptionalInt(body: Record<string, unknown>, name: string): number | undefined {
  const value = body[name];
  return value === undefined || value === null ? undefined : bodyInt(body, name);
}
