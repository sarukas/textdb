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

export function bodyOptionalInt(body: Record<string, unknown>, name: string): number | undefined {
  const value = body[name];
  return value === undefined || value === null ? undefined : bodyInt(body, name);
}
