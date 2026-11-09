'use client';

/**
 * 服务端远程表：实现 db.ts / 组件实际用到的 Dexie EntityTable 子集。
 *
 * 保留 Dexie 的链式调用形态（orderBy().reverse().limit().toArray() 等），
 * 这样上层组件与 db.ts 的封装函数无需感知存储位置变化。
 * 数据只在内存镜像中，持久化由 server-data.ts 负责，不写浏览器存储。
 */

import {
  deleteEntries,
  getEntry,
  getItems,
  hydrateServerData,
  putEntries,
  clearSection,
  type DataSection,
} from './server-data';

type SortDir = 1 | -1;

interface Filter {
  field: string;
  value: unknown;
}

class RemoteCollection<T> {
  constructor(
    private readonly load: () => Promise<T[]>,
    private readonly sortField: string | null = null,
    private readonly dir: SortDir = 1,
    private readonly limitN: number | null = null,
    private readonly filter: Filter | null = null
  ) {}

  reverse(): RemoteCollection<T> {
    return new RemoteCollection<T>(
      this.load,
      this.sortField,
      (this.dir * -1) as SortDir,
      this.limitN,
      this.filter
    );
  }

  limit(n: number): RemoteCollection<T> {
    return new RemoteCollection<T>(this.load, this.sortField, this.dir, n, this.filter);
  }

  async count(): Promise<number> {
    return (await this.toArray()).length;
  }

  async toArray(): Promise<T[]> {
    let rows = await this.load();
    if (this.filter) {
      const { field, value } = this.filter;
      rows = rows.filter(
        (r) => (r as Record<string, unknown>)[field] === value
      );
    }
    if (this.sortField) {
      const field = this.sortField;
      const dir = this.dir;
      rows = [...rows].sort((a, b) => {
        const av = (a as Record<string, unknown>)[field];
        const bv = (b as Record<string, unknown>)[field];
        if (typeof av === 'number' && typeof bv === 'number') {
          return (av - bv) * dir;
        }
        return String(av ?? '').localeCompare(String(bv ?? '')) * dir;
      });
    }
    if (this.limitN !== null) rows = rows.slice(0, this.limitN);
    return rows;
  }
}

export class RemoteTable<T extends object> {
  constructor(
    private readonly section: DataSection,
    private readonly keyField: string
  ) {}

  private keyOf(item: T): string {
    const v = (item as Record<string, unknown>)[this.keyField];
    return v === undefined || v === null ? '' : String(v);
  }

  private async rows(): Promise<T[]> {
    await hydrateServerData();
    return getItems<T>(this.section);
  }

  async get(key: string): Promise<T | undefined> {
    await hydrateServerData();
    return getEntry<T>(this.section, key);
  }

  async put(item: T): Promise<string> {
    const key = this.keyOf(item);
    await putEntries(this.section, { [key]: item });
    return key;
  }

  async bulkPut(items: T[]): Promise<string> {
    const entries: Record<string, unknown> = {};
    let last = '';
    for (const item of items) {
      const key = this.keyOf(item);
      entries[key] = item;
      last = key;
    }
    await putEntries(this.section, entries);
    return last;
  }

  async update(key: string, changes: Partial<T>): Promise<number> {
    const current = await this.get(key);
    if (!current) return 0;
    await putEntries(this.section, { [key]: { ...current, ...changes } });
    return 1;
  }

  async delete(key: string): Promise<void> {
    await deleteEntries(this.section, [key]);
  }

  async bulkDelete(keys: string[]): Promise<void> {
    await deleteEntries(this.section, keys);
  }

  async clear(): Promise<void> {
    await clearSection(this.section);
  }

  async count(): Promise<number> {
    return (await this.rows()).length;
  }

  toArray(): Promise<T[]> {
    return this.rows();
  }

  orderBy(field: string): RemoteCollection<T> {
    return new RemoteCollection<T>(() => this.rows(), field);
  }

  where(field: string): { equals: (value: unknown) => RemoteCollection<T> } {
    const load = () => this.rows();
    return {
      equals: (value: unknown) =>
        new RemoteCollection<T>(load, null, 1, null, { field, value }),
    };
  }
}
