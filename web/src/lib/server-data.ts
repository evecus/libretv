'use client';

/**
 * 服务端持久化数据层。
 *
 * 设计要点：
 * - 启动时一次 GET /api/data 拉全量到内存镜像，之后所有读都走内存（不落浏览器存储）；
 * - 写操作先更新内存，再经串行队列 PUT/DELETE 到服务端，保证顺序不错乱；
 * - zustand persist 与 Dexie 风格的表都通过这里的适配层访问，组件代码无需改动；
 * - 缓存（搜索/EPG/画质/测活/分片）不经过这里，仍留在内存或浏览器 Cache Storage。
 *
 * 与 client-api.ts 共用 401 全局事件名，未登录时统一弹出登录框。
 */

export const DATA_SECTIONS = ['history', 'progress', 'searchHistory', 'settings'] as const;
export type DataSection = (typeof DATA_SECTIONS)[number];

export const UNAUTHORIZED_EVENT = 'libretv:unauthorized';
/** 服务端数据拉取完成（用于通知 store 重新 hydrate） */
export const SERVER_DATA_EVENT = 'libretv:serverdata';

type SectionMap = Record<string, unknown>;
type Snapshot = Record<DataSection, SectionMap>;

function emptySnapshot(): Snapshot {
  return { history: {}, progress: {}, searchHistory: {}, settings: {} };
}

let snapshot: Snapshot = emptySnapshot();
let hydratePromise: Promise<void> | null = null;
let writeQueue: Promise<void> = Promise.resolve();
let ready = false;

export function serverDataReady(): boolean {
  return ready;
}

/** 全量 hydrate；重复调用复用同一个 Promise。force=true 用于登录成功后重试。 */
export function hydrateServerData(force = false): Promise<void> {
  if (hydratePromise && !force) return hydratePromise;
  if (force) hydratePromise = null;
  hydratePromise = (async () => {
    try {
      const res = await fetch('/api/data', {
        credentials: 'same-origin',
        cache: 'no-store',
      });
      if (res.status === 401 || res.status === 503) {
        // 未登录/未配置密码：交由全局登录框处理，登录后 reloadServerData 重试
        window.dispatchEvent(
          new CustomEvent(UNAUTHORIZED_EVENT, {
            detail: res.status === 503 ? 'setup' : undefined,
          })
        );
        return;
      }
      if (!res.ok) return;
      const body = (await res.json()) as {
        sections?: Record<string, Record<string, unknown>>;
      };
      const next = emptySnapshot();
      for (const section of DATA_SECTIONS) {
        const raw = body.sections?.[section];
        if (raw && typeof raw === 'object' && !Array.isArray(raw)) {
          Object.assign(next[section], raw);
        }
      }
      snapshot = next;
      ready = true;
      window.dispatchEvent(new CustomEvent(SERVER_DATA_EVENT));
    } catch {
      // 网络异常：本会话以空数据运行，不降级到浏览器存储
    }
  })();
  return hydratePromise;
}

/** 登录成功后重新拉取，并通知各订阅者（store 会重新 rehydrate） */
export async function reloadServerData(): Promise<void> {
  await hydrateServerData(true);
  window.dispatchEvent(new CustomEvent(SERVER_DATA_EVENT));
}

export function onServerDataChanged(listener: () => void): () => void {
  const fn = () => listener();
  window.addEventListener(SERVER_DATA_EVENT, fn);
  return () => window.removeEventListener(SERVER_DATA_EVENT, fn);
}

export function getSection(section: DataSection): SectionMap {
  return snapshot[section];
}

export function getItems<T = unknown>(section: DataSection): T[] {
  return Object.values(snapshot[section]) as T[];
}

export function getEntry<T = unknown>(section: DataSection, key: string): T | undefined {
  return snapshot[section][key] as T | undefined;
}

/** 写请求串行化，避免并发 PUT 乱序覆盖 */
function enqueue(task: () => Promise<void>): Promise<void> {
  writeQueue = writeQueue.then(task).catch(() => {
    /* 单个写失败不阻断后续队列 */
  });
  return writeQueue;
}

async function write(
  section: DataSection,
  method: 'PUT' | 'DELETE',
  body?: unknown
): Promise<void> {
  const res = await fetch(`/api/data/${section}`, {
    method,
    credentials: 'same-origin',
    // keepalive：页面关闭前的最后一次 flush 也能送达
    keepalive: true,
    headers: body === undefined ? undefined : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (res.status === 401 || res.status === 503) {
    window.dispatchEvent(
      new CustomEvent(UNAUTHORIZED_EVENT, {
        detail: res.status === 503 ? 'setup' : undefined,
      })
    );
  }
}

export function putEntries(
  section: DataSection,
  entries: Record<string, unknown>
): Promise<void> {
  Object.assign(snapshot[section], entries);
  return enqueue(() => write(section, 'PUT', { items: entries }));
}

export function deleteEntries(section: DataSection, keys: string[]): Promise<void> {
  for (const key of keys) delete snapshot[section][key];
  if (keys.length === 0) return Promise.resolve();
  return enqueue(() => write(section, 'DELETE', { keys }));
}

export function clearSection(section: DataSection): Promise<void> {
  snapshot[section] = {};
  return enqueue(() => write(section, 'DELETE'));
}

/** 整段替换（配置导入用）：先清空再批量写入，仍在同一个串行队列中 */
export function replaceSection(
  section: DataSection,
  entries: Record<string, unknown>
): Promise<void> {
  snapshot[section] = { ...entries };
  return enqueue(async () => {
    await write(section, 'DELETE');
    if (Object.keys(entries).length > 0) {
      await write(section, 'PUT', { items: entries });
    }
  });
}

/** settings 分区的同步 Storage 适配器，供 zustand persist / 下载设置使用 */
export interface SyncStorage {
  getItem(name: string): string | null;
  setItem(name: string, value: string): void;
  removeItem(name: string): void;
}

export function createServerStorage(): SyncStorage {
  return {
    getItem: (name) => {
      const v = snapshot.settings[name];
      return typeof v === 'string' ? v : v === undefined ? null : JSON.stringify(v);
    },
    setItem: (name, value) => {
      void putEntries('settings', { [name]: value });
    },
    removeItem: (name) => {
      void deleteEntries('settings', [name]);
    },
  };
}

/** 供 download-settings.ts 等同步读取的辅助函数 */
export function readSetting(name: string): string | null {
  return createServerStorage().getItem(name);
}

export function writeSetting(name: string, value: string): void {
  createServerStorage().setItem(name, value);
}
