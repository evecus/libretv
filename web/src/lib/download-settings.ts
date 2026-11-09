/**
 * 下载相关设置：与其余设置一起存服务端 SQLite（settings 分区）。
 *
 * 与视频缓存设置（video-cache.ts）分开放：两者语义与生命周期无关，
 * 共用一个 key 会让「清空视频缓存」之类的操作意外重置下载偏好。
 */

import { readSetting, writeSetting } from './server-data';

const SETTINGS_KEY = 'libretv-download-settings';

export interface DownloadSettings {
  /**
   * 经服务器中转下载：m3u8 与分片全部经 /api/proxy 转发，用于直连被 CORS /
   * 防盗链 / 混合内容拦下的源。开启后整集流量都会经过服务器，
   * 带宽成本由部署者承担，因此默认关闭、由用户显式开启。
   */
  proxy: boolean;
}

export const DEFAULT_DOWNLOAD_SETTINGS: DownloadSettings = { proxy: false };

export function normalizeDownloadSettings(input: unknown): DownloadSettings {
  const raw = (input ?? {}) as Partial<DownloadSettings>;
  return { proxy: raw.proxy === true };
}

export function loadDownloadSettings(): DownloadSettings {
  try {
    const raw = readSetting(SETTINGS_KEY);
    return normalizeDownloadSettings(raw ? JSON.parse(raw) : undefined);
  } catch {
    return { ...DEFAULT_DOWNLOAD_SETTINGS };
  }
}

export function saveDownloadSettings(next: Partial<DownloadSettings>): DownloadSettings {
  const merged = normalizeDownloadSettings({ ...loadDownloadSettings(), ...next });
  try {
    writeSetting(SETTINGS_KEY, JSON.stringify(merged));
  } catch {
    /* 写入失败忽略，内存镜像仍可用 */
  }
  return merged;
}
