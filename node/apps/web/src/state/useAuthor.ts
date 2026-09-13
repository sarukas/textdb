import { useCallback, useState } from "react";

const KEY = "textdb.author";
export const DEFAULT_AUTHOR = "human";

function read(): string {
  try {
    return localStorage.getItem(KEY) || DEFAULT_AUTHOR;
  } catch {
    return DEFAULT_AUTHOR;
  }
}

export function useAuthor(): [string, (name: string) => void] {
  const [author, setAuthor] = useState(read);
  const update = useCallback((name: string) => {
    setAuthor(name);
    try {
      if (name.trim()) localStorage.setItem(KEY, name.trim());
      else localStorage.removeItem(KEY);
    } catch {
      // Storage unavailable: the name lasts for this session only.
    }
  }, []);
  return [author, update];
}

/** The author name to send: the input's value, or the default when it is blank. */
export function effectiveAuthor(name: string): string {
  return name.trim() || DEFAULT_AUTHOR;
}
