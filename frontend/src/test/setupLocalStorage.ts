/**
 * Node >= 22.4 (this repo runs Node 26) exposes a stub `localStorage` global
 * that returns `undefined` — with an ExperimentalWarning — unless the process
 * was started with `--localstorage-file`. Vitest's jsdom environment refuses
 * to overwrite globals that already exist on `globalThis` (see
 * `getWindowKeys` in vitest's `populateGlobal`: a key present on the global
 * is only replaced when it's on vitest's built-in KEYS list, and
 * `localStorage` isn't), so jsdom's working `localStorage` never gets
 * installed and any code touching it — e.g. every zustand `persist` store —
 * crashes with `Cannot read properties of undefined (reading 'setItem')`.
 *
 * Install a minimal in-memory `Storage` polyfill in its place. Vitest spins
 * up a fresh environment per test file, so each file gets a fresh store —
 * matching the per-file isolation jsdom's localStorage would have provided.
 * (`sessionStorage` is unaffected: Node's built-in is in-memory and works.)
 */
class MemoryStorage implements Storage {
  private map = new Map<string, string>();

  get length(): number {
    return this.map.size;
  }

  clear(): void {
    this.map.clear();
  }

  getItem(key: string): string | null {
    const k = String(key);
    return this.map.has(k) ? this.map.get(k)! : null;
  }

  key(index: number): string | null {
    return [...this.map.keys()][index] ?? null;
  }

  removeItem(key: string): void {
    this.map.delete(String(key));
  }

  setItem(key: string, value: string): void {
    this.map.set(String(key), String(value));
  }
}

Object.defineProperty(globalThis, "localStorage", {
  configurable: true,
  writable: true,
  value: new MemoryStorage(),
});
