import { badRequest } from './errors.ts';

/**
 * Resolve the conflicts `textdb sync` marked in a file by keeping one side of each: `textdb`
 * (between `<<<<<<<` and `=======`) or `disk` (between `=======` and `>>>>>>>`). Everything
 * outside the markers, already merged, stays; line endings are kept.
 */
export function resolveMarkers(text: string, keep: 'textdb' | 'disk'): string {
  let out = '';
  let inside: 'textdb' | 'disk' | null = null;
  for (const line of text.split(/(?<=\n)/)) {
    const bare = line.replace(/\r?\n$/, '');
    if (inside === null && bare.startsWith('<<<<<<< ')) {
      inside = 'textdb';
    } else if (inside === 'textdb' && bare === '=======') {
      inside = 'disk';
    } else if (inside === 'disk' && bare.startsWith('>>>>>>> ')) {
      inside = null;
    } else if (inside === null || inside === keep) {
      out += line;
    }
  }
  if (inside !== null) throw badRequest('the file has a conflict marker without its end; edit it by hand');
  return out;
}
