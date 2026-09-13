export type NodeKind = 'file' | 'folder';
export type CommitKind = 'direct' | 'rebased' | 'merged';
export type WriteKind = CommitKind | 'noop';
export type ChangeOp = 'create' | 'commit' | 'mkdir' | 'move' | 'delete';

export interface Info {
  db: string;
  files: number;
  last_seq: number;
}

export interface Entry {
  name: string;
  path: string;
  kind: NodeKind;
  nbytes: number | null;
  nlines: number | null;
  updated_at: string;
}

export interface FileView {
  path: string;
  version: number;
  head_version: number;
  content: string;
  nbytes: number;
  nlines: number;
  updated_at: string;
  updated_by: string | null;
}

export interface Chunk {
  ord: number;
  hash: string;
  byte_from: number;
  nbytes: number;
  line_from: number;
  nlines: number;
}

export interface HistoryEntry {
  version: number;
  author: string | null;
  ts: string;
  message: string | null;
  nbytes: number;
  /** Null for commits recorded by a build that predates commit kinds. */
  kind: CommitKind | null;
  base_version: number | null;
}

export interface Hunk {
  old_from: number;
  old_count: number;
  new_from: number;
  new_count: number;
  old_text: string;
  new_text: string;
}

export interface SearchHit {
  path: string;
  line: number;
  snippet: string;
  rank: number;
}

export interface WriteResult {
  version: number;
  kind: WriteKind;
}

export interface Change {
  seq: number;
  ts: string;
  op: ChangeOp;
  path: string;
  old_path: string | null;
  node_kind: NodeKind;
  version: number | null;
  base_version: number | null;
  commit_kind: CommitKind | null;
  author: string | null;
  message: string | null;
}
