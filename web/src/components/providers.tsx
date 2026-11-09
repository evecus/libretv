'use client';

import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { useEffect, useState, type ReactNode } from 'react';
import { ToastProvider } from './toast';
import { AuthProvider } from './auth';
import { ThemeProvider } from './theme';
import { GlobalDownloadManager } from './download-manager';
import { useAppStore, hydrateLiveProbeResults } from '@/lib/store';
import { api, STATUS_QUERY_KEY } from '@/lib/client-api';
import { applyEnvPresets } from '@/lib/subscription-sync';
import { hydrateServerData } from '@/lib/server-data';

export function Providers({ children }: { children: ReactNode }) {
  const [queryClient] = useState(
    () =>
      new QueryClient({
        defaultOptions: {
          queries: { retry: 1, refetchOnWindowFocus: false, staleTime: 60_000 },
        },
      })
  );

  // store 配置了 skipHydration：等挂载后再读持久化快照，
  // 保证 hydration 阶段客户端与服务端渲染结果一致。
  useEffect(() => {
    let cancelled = false;
    (async () => {
      // 先把服务端 SQLite 中的配置/历史拉进内存镜像，再 rehydrate；
      // 顺序反了 zustand 会读到空快照。未登录时 /api/data 返回 401 并弹出登录框，
      // 登录成功后由 AuthProvider 调 reloadServerData 重拉并触发重新 rehydrate。
      await hydrateServerData();
      if (cancelled) return;
      await useAppStore.persist.rehydrate();
      if (cancelled) return;
      // 测活缓存仍从浏览器 IndexedDB 恢复（属于缓存，不存服务器）
      void hydrateLiveProbeResults();
      try {
        // 拉取部署者通过 DEFAULT_SOURCES 等预置的源；与 AuthProvider 共用 query key，全站只发一次
        const d = await queryClient.fetchQuery({
          queryKey: STATUS_QUERY_KEY,
          queryFn: () => api.status(),
        });
        // 预置订阅要经鉴权接口拉取，首屏这次可能发生在登录之前而 401 静默失败；
        // 登录成功后由 AuthProvider 再调一次 applyEnvPresets 补齐（函数幂等）
        if (d && !cancelled) await applyEnvPresets(d);
      } catch {
        // 失败静默
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [queryClient]);

  return (
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
        <ToastProvider>
          {/* 全站常驻：下载事件监听（enqueueDownload）依赖它存在——
              挂在 Header 里会让 /watch 等不渲染 Header 的页面派发的事件凭空丢失 */}
          <GlobalDownloadManager />
          <AuthProvider>{children}</AuthProvider>
        </ToastProvider>
      </ThemeProvider>
    </QueryClientProvider>
  );
}
