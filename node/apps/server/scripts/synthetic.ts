/** A deterministic synthetic markdown corpus for exercising the live app. */

export interface Doc {
  path: string;
  content: string;
}

export type Random = () => number;

/** mulberry32: small, fast and reproducible from a seed. */
export function random(seed: number): Random {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const WORDS = (
  'account agent api archive backlog baseline batch branch budget cache change channel checkpoint client cluster ' +
  'commit config contract corpus cursor customer dashboard deploy design diff document draft edge engine event ' +
  'feature feed field filter folder graph handler history import index input job kernel ledger limit line log ' +
  'merge metric migration model module monitor network node owner page parser partner patch pipeline plan policy ' +
  'process queue quota rebase record region release report request review risk roadmap route runbook schema ' +
  'search section segment server service session snapshot source stage store stream summary sync table target ' +
  'team template tenant test ticket timeline token trace tree update usage version view watcher window workflow'
).split(' ');

const AREAS = ['guides', 'reference', 'runbooks', 'design', 'meetings', 'research', 'teams', 'api'];
const OWNERS = ['alice', 'bob', 'carol', 'dmitri', 'eve', 'farah', 'goran', 'hana'];
const STATUSES = ['draft', 'review', 'published', 'archived'];

export function pick<T>(rand: Random, items: readonly T[]): T {
  return items[Math.floor(rand() * items.length)]!;
}

function int(rand: Random, min: number, max: number): number {
  return min + Math.floor(rand() * (max - min + 1));
}

function words(rand: Random, n: number): string[] {
  return Array.from({ length: n }, () => pick(rand, WORDS));
}

function titleCase(parts: string[]): string {
  return parts.map((w) => w[0]!.toUpperCase() + w.slice(1)).join(' ');
}

export function sentence(rand: Random): string {
  const text = words(rand, int(rand, 6, 16)).join(' ');
  return `${text[0]!.toUpperCase()}${text.slice(1)}.`;
}

export function paragraph(rand: Random): string {
  return Array.from({ length: int(rand, 2, 6) }, () => sentence(rand)).join(' ');
}

function list(rand: Random): string {
  const ordered = rand() < 0.3;
  return Array.from({ length: int(rand, 3, 7) }, (_, i) => `${ordered ? `${i + 1}.` : '-'} ${sentence(rand)}`).join('\n');
}

function codeBlock(rand: Random): string {
  const [a, b, c] = words(rand, 3);
  const samples = [
    ['ts', `export function ${a}${titleCase([b!])}(input: string): string {\n  const ${c} = input.trim();\n  return ${c}.toUpperCase();\n}`],
    ['sql', `SELECT ${a}, count(*) AS n\nFROM ${b}\nWHERE ${c} IS NOT NULL\nGROUP BY ${a}\nORDER BY n DESC;`],
    ['bash', `textdb import ./${a} --db kb.db\ntextdb ls /${b}\ntextdb cat /${b}/${c}.md | head -20`],
    ['yaml', `${a}:\n  ${b}: ${int(rand, 1, 100)}\n  ${c}: [${words(rand, 3).join(', ')}]`],
  ] as const;
  const [lang, body] = pick(rand, samples);
  return `\`\`\`${lang}\n${body}\n\`\`\``;
}

function table(rand: Random): string {
  const cols = words(rand, 3);
  const rows = Array.from({ length: int(rand, 2, 5) }, () => `| ${words(rand, 3).join(' | ')} |`);
  return [`| ${cols.join(' | ')} |`, `|${' --- |'.repeat(cols.length)}`, ...rows].join('\n');
}

function section(rand: Random, level: number): string {
  const blocks = [`${'#'.repeat(level)} ${titleCase(words(rand, int(rand, 2, 4)))}`, paragraph(rand)];
  for (let i = int(rand, 1, 4); i > 0; i--) {
    const roll = rand();
    if (roll < 0.35) blocks.push(paragraph(rand));
    else if (roll < 0.65) blocks.push(list(rand));
    else if (roll < 0.85) blocks.push(codeBlock(rand));
    else blocks.push(table(rand));
  }
  if (level === 2 && rand() < 0.4) blocks.push(section(rand, 3));
  return blocks.join('\n\n');
}

export function document(rand: Random, title: string, minBytes = 0): string {
  const date = `2026-${String(int(rand, 1, 9)).padStart(2, '0')}-${String(int(rand, 1, 28)).padStart(2, '0')}`;
  const frontmatter = [
    '---',
    `title: ${title}`,
    `owner: ${pick(rand, OWNERS)}`,
    `status: ${pick(rand, STATUSES)}`,
    `tags: [${words(rand, int(rand, 1, 4)).join(', ')}]`,
    `updated: ${date}`,
    '---',
  ].join('\n');
  const parts = [frontmatter, `# ${title}`, paragraph(rand)];
  let size = parts.join('\n\n').length;
  for (let n = int(rand, 3, 8); n > 0 || size < minBytes; n--) {
    const next = section(rand, 2);
    parts.push(next);
    size += next.length + 2;
  }
  return `${parts.join('\n\n')}\n`;
}

/** `count` documents under nested folders; roughly one in 500 (at least three) is ~200 KB. */
export function* syntheticCorpus(count: number, seed: number): Generator<Doc> {
  const rand = random(seed);
  const folders = AREAS.flatMap((area) =>
    Array.from({ length: int(rand, 3, 6) }, () => {
      const sub = `/${area}/${words(rand, 2).join('-')}`;
      return rand() < 0.4 ? [sub, `${sub}/${pick(rand, WORDS)}`] : [sub];
    }).flat(),
  );
  const longEvery = Math.max(1, Math.floor(count / Math.max(3, Math.floor(count / 500))));
  for (let i = 0; i < count; i++) {
    const nameWords = words(rand, int(rand, 1, 3));
    const folder = i === 0 ? '' : pick(rand, folders);
    const name = i === 0 ? 'index' : `${nameWords.join('-')}-${i}`;
    const long = i % longEvery === longEvery - 1;
    yield { path: `${folder}/${name}.md`, content: document(rand, titleCase(nameWords), long ? 200_000 : 0) };
  }
}
