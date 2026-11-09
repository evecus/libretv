import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/**
 * 服务端持久化数据层测试。
 *
 * 旧版 persist-storage.test.ts 验证的是「多标签共享 localStorage + storage 事件」
 * 的节流/重放冲突处理；数据迁移到服务端 SQLite 后该机制已不存在
 * （服务端是唯一真源，写请求经串行队列发送），故改为验证新的数据层契约。
 */

type Call = { url: string; method?: string; body?: string };

function jsonResponse(body: unknown) {
  return { status: 200, ok: true, json: async () => body };
}

async function fresh(handler: (url: string, init?: RequestInit) => unknown) {
  vi.resetModules();
  const calls: Call[] = [];
  const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
    if (init?.method) {
      calls.push({
        url,
        method: init.method,
        body: typeof init.body === 'string' ? init.body : undefined,
      });
    }
    return handler(url, init);
  });
  vi.stubGlobal('fetch', fetchMock);
  vi.stubGlobal('window', new EventTarget());
  const mod = await import('./server-data');
  return { mod, calls, fetchMock };
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe('server-data 全量 hydrate', () => {
  it('拉取全量并填充四个分区，设置快照可同步读取', async () => {
    const snapshot = '{"state":{"adFilter":false},"version":1}';
    const { mod } = await fresh(() =>
      jsonResponse({
        sections: {
          history: { a: { id: 'a', timestamp: 1 } },
          settings: { 'libretv-settings': snapshot },
        },
      })
    );
    await mod.hydrateServerData();
    expect(mod.serverDataReady()).toBe(true);
    expect(mod.getItems('history')).toEqual([{ id: 'a', timestamp: 1 }]);
    expect(mod.readSetting('libretv-settings')).toBe(snapshot);
    expect(mod.getItems('progress')).toEqual([]);
    expect(mod.getItems('searchHistory')).toEqual([]);
  });

  it('未登录 401 派发全局登录事件且不写入数据', async () => {
    const { mod } = await fresh(() => ({ status: 401, ok: false }));
    let fired = 0;
    window.addEventListener('libretv:unauthorized', () => {
      fired += 1;
    });
    await mod.hydrateServerData();
    expect(fired).toBe(1);
    expect(mod.serverDataReady()).toBe(false);
  });

  it('重复 hydrate 复用同一请求，登录后 force 重拉', async () => {
    const { mod, fetchMock } = await fresh(() => jsonResponse({ sections: {} }));
    await mod.hydrateServerData();
    await mod.hydrateServerData();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    await mod.reloadServerData();
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });
});

describe('server-data 写入', () => {
  it('createServerStorage 同步读写内存镜像，且不经 localStorage', async () => {
    const { mod } = await fresh(() => jsonResponse({ sections: {} }));
    await mod.hydrateServerData();
    const storage = mod.createServerStorage();
    storage.setItem('k', 'v');
    expect(storage.getItem('k')).toBe('v');
    storage.removeItem('k');
    expect(storage.getItem('k')).toBeNull();
  });

  it('putEntries 先更新内存，再 PUT 到对应分区', async () => {
    const { mod, calls } = await fresh(() => jsonResponse({ ok: true }));
    await mod.hydrateServerData();
    await mod.putEntries('history', { a: { id: 'a', title: 'A' } });
    expect(mod.getEntry('history', 'a')).toEqual({ id: 'a', title: 'A' });
    const put = calls.find((c) => c.method === 'PUT');
    expect(put?.url).toBe('/api/data/history');
    expect(JSON.parse(put!.body!)).toEqual({ items: { a: { id: 'a', title: 'A' } } });
  });

  it('写请求串行化，顺序与调用顺序一致', async () => {
    const { mod, calls } = await fresh(() => jsonResponse({ ok: true }));
    await mod.hydrateServerData();
    await Promise.all([
      mod.putEntries('history', { a: { id: 'a' } }),
      mod.deleteEntries('history', ['a']),
      mod.clearSection('history'),
    ]);
    expect(calls.map((c) => c.method)).toEqual(['PUT', 'DELETE', 'DELETE']);
  });

  it('deleteEntries 按 key 删除，clearSection 清空整段', async () => {
    const { mod } = await fresh(() => jsonResponse({ sections: {} }));
    await mod.hydrateServerData();
    await mod.putEntries('history', { a: { id: 'a' }, b: { id: 'b' } });
    await mod.deleteEntries('history', ['a']);
    expect(mod.getItems('history')).toEqual([{ id: 'b' }]);
    await mod.clearSection('history');
    expect(mod.getItems('history')).toEqual([]);
  });
});

describe('RemoteTable（Dexie 接口子集）', () => {
  it('get/put/count/orderBy/reverse/limit/delete/clear', async () => {
    const { mod } = await fresh(() => jsonResponse({ sections: {} }));
    await mod.hydrateServerData();
    const { RemoteTable } = await import('./remote-table');
    const table = new RemoteTable<{ id: string; timestamp: number }>('history', 'id');
    await table.bulkPut([
      { id: 'a', timestamp: 1 },
      { id: 'b', timestamp: 2 },
      { id: 'c', timestamp: 3 },
    ]);
    expect(await table.count()).toBe(3);
    expect((await table.get('b'))?.timestamp).toBe(2);
    expect(
      (await table.orderBy('timestamp').reverse().limit(2).toArray()).map((r) => r.id)
    ).toEqual(['c', 'b']);
    expect((await table.orderBy('timestamp').limit(2).toArray()).map((r) => r.id)).toEqual([
      'a',
      'b',
    ]);
    await table.update('a', { timestamp: 9 });
    expect((await table.get('a'))?.timestamp).toBe(9);
    await table.delete('a');
    expect(await table.count()).toBe(2);
    await table.clear();
    expect(await table.count()).toBe(0);
  });
});
