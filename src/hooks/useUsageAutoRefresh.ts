import { useCallback, useEffect, useRef } from "react";
import { focusManager, useQueryClient } from "@tanstack/react-query";
import { usageApi } from "@/lib/api/usage";
import { usageKeys } from "@/lib/query/usage";
import type { SessionSyncResult } from "@/types/usage";

interface UsageAutoRefreshOptions {
  refreshIntervalMs: number;
  sessionAutoSyncEnabled: boolean;
  paused?: boolean;
  /** 时间范围随「现在」移动（今天、最近 24 小时等） */
  rollingRange?: boolean;
  /**
   * 远程 SSH 主机 id：用量页切到远程时本机扫描停跑（数据在远端），自动刷新
   * 改走轻路径——定时 invalidate 后由查询重跑触发命令层的清单 diff 轻同步，
   * 不直接调 force。
   */
  remoteHostId?: string | null;
}

/** 随时间移动的范围多久重算一次：跨过午夜、滚动窗口移出旧记录都靠它 */
const ROLLING_RANGE_REFRESH_MS = 60_000;

/**
 * 用量页打开期间按所选间隔扫描会话日志：打开即扫一次，之后每隔一段扫一次，
 * 窗口隐藏时停、重新显示时立刻补扫。后台只有 10 分钟一次的兜底扫描。
 * 导入新用量后后端会发 `usage-log-recorded`，统计由 useUsageEventBridge 刷新；
 * 这里每轮只刷新「上次同步」时间，没有新数据时不重读整页。
 * 范围随时间移动时另有每分钟一次的重查，否则没有新用量时跨过午夜「今天」还停在昨天。
 * 远程主机下（remoteHostId）没有可扫的本机会话：改为按同一间隔失效 usage
 * 查询，让带 hostId 的命令在查询里内建完成轻同步（清单 diff + 增量）。
 */
export function useUsageAutoRefresh({
  refreshIntervalMs,
  sessionAutoSyncEnabled,
  paused = false,
  rollingRange = false,
  remoteHostId = null,
}: UsageAutoRefreshOptions) {
  const queryClient = useQueryClient();
  const pendingSync = useRef<Promise<SessionSyncResult> | null>(null);

  // 手动同步复用尚未完成的静默扫描；慢扫描不会在后端互斥锁前排队累积。
  const syncSessionUsage = useCallback(() => {
    if (!pendingSync.current) {
      pendingSync.current = usageApi.syncSessionUsage().finally(() => {
        pendingSync.current = null;
      });
    }
    return pendingSync.current;
  }, []);

  useEffect(() => {
    if (!sessionAutoSyncEnabled || refreshIntervalMs <= 0 || paused) return;
    // 远程视图：没有本机会话可扫，轻刷新由下面的远程 effect 接管
    if (remoteHostId) return;
    let disposed = false;

    const scan = async () => {
      if (!focusManager.isFocused() || pendingSync.current) return;
      try {
        const result = await syncSessionUsage();
        if (result.errors.length > 0) {
          console.warn(
            "[UsageDashboard] Session auto-sync errors",
            result.errors,
          );
        }
      } catch (error) {
        // 静默扫描失败不弹提示，下一轮继续重试
        console.warn("[UsageDashboard] Session auto-sync failed", error);
      }
      if (!disposed) {
        await queryClient.invalidateQueries({
          queryKey: [...usageKeys.all, "session-last-sync"],
        });
      }
    };

    void scan();
    const timer = window.setInterval(() => void scan(), refreshIntervalMs);
    const unsubscribe = focusManager.subscribe((focused) => {
      if (focused) void scan();
    });
    return () => {
      disposed = true;
      window.clearInterval(timer);
      unsubscribe();
    };
  }, [
    paused,
    queryClient,
    refreshIntervalMs,
    remoteHostId,
    sessionAutoSyncEnabled,
    syncSessionUsage,
  ]);

  // 远程轻刷新：定时（含重新聚焦补一次）失效 usage 查询。查询重跑时带
  // hostId 的命令会在后端先跑清单 diff，无变化零流量、有变化增量拉取。
  useEffect(() => {
    if (!remoteHostId || paused || refreshIntervalMs <= 0) return;
    const refresh = () => {
      if (focusManager.isFocused()) {
        void queryClient.invalidateQueries({ queryKey: usageKeys.all });
      }
    };
    const timer = window.setInterval(refresh, refreshIntervalMs);
    const unsubscribe = focusManager.subscribe((focused) => {
      if (focused) refresh();
    });
    return () => {
      window.clearInterval(timer);
      unsubscribe();
    };
  }, [paused, queryClient, refreshIntervalMs, remoteHostId]);

  useEffect(() => {
    if (!rollingRange || paused) return;
    const timer = window.setInterval(() => {
      if (focusManager.isFocused()) {
        void queryClient.invalidateQueries({ queryKey: usageKeys.all });
      }
    }, ROLLING_RANGE_REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [paused, queryClient, rollingRange]);

  return { syncSessionUsage };
}
