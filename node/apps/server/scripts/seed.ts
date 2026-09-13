/** npm run seed -- --db <path> --files 2000 [--seed 1] [--batch 250] */
import { parseArgs } from 'node:util';
import { openCorpus } from '@textdb/node';
import { type Doc, syntheticCorpus } from './synthetic.ts';

const { values } = parseArgs({
  options: {
    db: { type: 'string', default: process.env.TEXTDB_DB || './kb.db' },
    files: { type: 'string', default: '2000' },
    seed: { type: 'string', default: '1' },
    batch: { type: 'string', default: '250' },
  },
});

const files = Number(values.files);
const batchSize = Number(values.batch);
const kb = openCorpus({ db: values.db, author: 'seed' });
const started = performance.now();
let written = 0;
let bytes = 0;

function writeBatch(docs: Doc[]): void {
  kb.transaction(() => {
    for (const doc of docs) kb.write(doc.path, doc.content, { message: 'seed' });
  });
  written += docs.length;
  console.log(`${written}/${files} files`);
}

let batch: Doc[] = [];
for (const doc of syntheticCorpus(files, Number(values.seed))) {
  batch.push(doc);
  bytes += Buffer.byteLength(doc.content);
  if (batch.length === batchSize) {
    writeBatch(batch);
    batch = [];
  }
}
if (batch.length > 0) writeBatch(batch);

const seconds = (performance.now() - started) / 1000;
const { files: total, last_seq } = kb.info();
console.log(
  `seeded ${written} files, ${(bytes / 1024 / 1024).toFixed(1)} MiB in ${seconds.toFixed(1)} s ` +
    `into ${kb.db} (${total} files, last_seq ${last_seq})`,
);
kb.close();
