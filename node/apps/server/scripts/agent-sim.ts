/**
 * npm run agent-sim -- --db <path> --path /some/file.md [--interval 1500] [--author agent-sim]
 *
 * Simulates an agent editing one document from a separate process: every ~interval it reads the
 * file and makes a small line edit with `textdb_replace_lines` against the version it read.
 */
import { parseArgs } from 'node:util';
import { Conflict, NotFound, TextdbError, openCorpus } from '@textdb/node';
import { type Random, document, paragraph, pick, random, sentence } from './synthetic.ts';

const { values } = parseArgs({
  options: {
    db: { type: 'string', default: process.env.TEXTDB_DB || './kb.db' },
    path: { type: 'string' },
    interval: { type: 'string', default: '1500' },
    author: { type: 'string', default: 'agent-sim' },
  },
});
if (!values.path) {
  console.error('usage: npm run agent-sim -- --db <path> --path /some/file.md [--interval 1500]');
  process.exit(2);
}
if (!values.path.startsWith('/')) {
  // Git Bash rewrites a leading-slash argument into a Windows path.
  console.error(`--path must start with "/", got ${values.path} (in Git Bash, set MSYS_NO_PATHCONV=1)`);
  process.exit(2);
}
const filePath = values.path;
const interval = Number(values.interval);
const rand = random(Date.now());
const kb = openCorpus({ db: values.db, author: values.author });

interface LineEdit {
  from: number;
  to: number;
  text: string;
  label: string;
}

function splitLines(content: string): string[] {
  const lines = content.split('\n');
  if (lines.at(-1) === '') lines.pop();
  return lines;
}

/** Lines outside the frontmatter and code fences that hold prose. */
function editableLines(lines: string[]): number[] {
  const result: number[] = [];
  let inFence = false;
  let inFrontmatter = lines[0] === '---';
  lines.forEach((line, i) => {
    if (inFrontmatter) {
      if (i > 0 && line === '---') inFrontmatter = false;
      return;
    }
    if (line.startsWith('```')) inFence = !inFence;
    else if (!inFence && /\w/.test(line) && !line.startsWith('|')) result.push(i + 1);
  });
  return result;
}

function chooseEdit(lines: string[], rand: Random): LineEdit {
  const stamp = new Date().toLocaleTimeString('en-GB');
  const candidates = editableLines(lines);
  const roll = rand();
  if (candidates.length > 0 && roll < 0.45) {
    const line = pick(rand, candidates);
    // Keep the markdown marker (heading, bullet, number) and rewrite the prose after it.
    const marker = /^\s*(?:#{1,6}|[-*+]|\d+\.)\s+/.exec(lines[line - 1]!)?.[0] ?? '';
    return { from: line, to: line, text: `${marker}${sentence(rand)}\n`, label: `replace line ${line}` };
  }
  if (candidates.length > 0 && roll < 0.8) {
    const after = pick(rand, candidates);
    const count = 1 + Math.floor(rand() * 3);
    const text = Array.from({ length: count }, () => `- ${sentence(rand)}\n`).join('');
    return { from: after + 1, to: after, text, label: `insert ${count} line(s) after ${after}` };
  }
  const end = lines.length;
  return { from: end + 1, to: end, text: `\n**${values.author} ${stamp}:** ${paragraph(rand)}\n`, label: 'append a paragraph' };
}

function tick(): void {
  try {
    const file = kb.read(filePath);
    const edit = chooseEdit(splitLines(file.content), rand);
    const result = kb.replaceLines(filePath, edit.from, edit.to, edit.text, { baseVersion: file.version });
    console.log(`v${result.version} · ${result.kind} · ${edit.label}`);
  } catch (error) {
    if (error instanceof NotFound) {
      const result = kb.write(filePath, document(rand, 'Agent Scratchpad'), { message: 'agent-sim: create' });
      console.log(`created ${filePath} at v${result.version}`);
    } else if (error instanceof Conflict) {
      console.log(`conflict on lines ${error.payload.region_line_from}-${error.payload.region_line_to}; retrying on the next tick`);
    } else if (error instanceof TextdbError) {
      console.log(`${error.code} ${error.message}`);
    } else {
      throw error;
    }
  }
  timer = setTimeout(tick, interval * (0.6 + rand() * 0.8));
}

console.log(`agent-sim editing ${filePath} in ${kb.db} as ${values.author}; Ctrl+C to stop`);
let timer = setTimeout(tick, 0);
process.once('SIGINT', () => {
  clearTimeout(timer);
  kb.close();
  process.exit(0);
});
