import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { toast } from "@/lib/toast";
import {
  Check,
  ChartColumn,
  ChevronDown,
  ChevronRight,
  ListFilter,
  Loader2,
  RefreshCw,
  ListTree,
  Settings2,
  TriangleAlert,
} from "lucide-react";
import { AppPageHeader } from "@/components/shell/AppPageHeader";
import { AppGlyph, APP_DISPLAY_NAME } from "@/components/shell/AppGlyph";
import { HelpTip } from "@/components/ui/help-tip";
import { PageTabs } from "@/components/ui/page-tabs";
import { Button } from "@/components/ui/button";
import { HoverTip } from "@/components/ui/hover-tip";
import { SessionHostMenu } from "@/components/sessions/SessionHostMenu";
import { normalizeHostId } from "@/components/sessions/utils";
import {
  SegmentThumb,
  useSlidingIndicator,
} from "@/components/ui/sliding-indicator";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuPortal,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { FullScreenPanel } from "@/components/common/FullScreenPanel";
import {
  KNOWN_APP_TYPES,
  type AppTypeFilter,
  type RemoteUsageSyncStatus,
  type UsageRangePreset,
  type UsageRangeSelection,
} from "@/types/usage";
import {
  usageKeys,
  UsageHostProvider,
  useModelStats,
  useProviderStats,
  useRequestLogs,
  useSessionUsageLastSync,
} from "@/lib/query/usage";
import { useSettingsQuery } from "@/lib/query";
import { DEFAULT_VISIBLE_APPS } from "@/config/appConfig";
import { useUsageEventBridge } from "@/hooks/useUsageEventBridge";
import { useUsageAutoRefresh } from "@/hooks/useUsageAutoRefresh";
import { usageApi } from "@/lib/api/usage";
import { getUsageRangePresetLabel, resolveUsageRange } from "@/lib/usageRange";
import { cn } from "@/lib/utils";
import { UsageHero } from "./UsageHero";
import { UsageTrendChart } from "./UsageTrendChart";
import { RequestLogTable } from "./RequestLogTable";
import { ProviderStatsTable } from "./ProviderStatsTable";
import { ModelStatsTable } from "./ModelStatsTable";
import { PricingConfigPanel } from "./PricingConfigPanel";
import { RequestDetailPanel } from "./RequestDetailPanel";
import { UsageSettingsSheet } from "./UsageSettingsSheet";
import { RequestLogColumnsMenu } from "./RequestLogColumnsMenu";
import { resolveRequestLogColumns } from "./requestLogColumns";
import type { UsageDisplaySettings } from "@/types";
import { UsageDateRangePicker } from "./UsageDateRangePicker";
import { UsageHeatmap } from "./UsageHeatmap";
import {
  LOG_PAGE_SIZES,
  type LogPageSize,
  USAGE_CHOICE_KEYS,
  USAGE_METRICS,
  type UsageMetric,
  readRememberedChoice,
  useRememberedChoice,
  writeRememberedChoice,
} from "./rememberedChoice";
import { fmtInt, formatRelativeTime, getLocaleFromLanguage } from "./format";
import { getUsageProviderLabel, usageProviderTitle } from "./providerLabel";
import { useElementWidth } from "./usageTable";

// 用量页打开时扫描会话日志的间隔；旧版存过的「关」(0) 等无效值回落到默认
const DEFAULT_REFRESH_INTERVAL_MS = 60000;
const REFRESH_INTERVAL_OPTIONS_MS = [5000, 10000, 30000, 60000] as const;
type RefreshIntervalOption = (typeof REFRESH_INTERVAL_OPTIONS_MS)[number];

const isRefreshIntervalOption = (
  value: number | undefined,
): value is RefreshIntervalOption =>
  REFRESH_INTERVAL_OPTIONS_MS.includes(value as RefreshIntervalOption);

const normalizeRefreshInterval = (value: number | undefined) =>
  isRefreshIntervalOption(value) ? value : DEFAULT_REFRESH_INTERVAL_MS;

const STATUS_CODE_OPTIONS = [200, 400, 401, 429, 500] as const;

// 页签只放数据视图；显示和定价设置在「用量设置」抽屉里
type UsageTab = "logs" | "providers" | "models";
const TABS: UsageTab[] = ["logs", "providers", "models"];
/** 记住的时间范围只认预设：自定义的起止日期过几天就不是用户想看的了，选自定义时不改记下的预设 */
const REMEMBERED_RANGE_PRESETS: UsageRangePreset[] = [
  "today",
  "1d",
  "7d",
  "14d",
  "30d",
  "all",
];

/**
 * 手动「立即同步」的时间，离开页面再回来也还在。后端也会记下最近一次扫描（后台定时和
 * 手动都算）完成的时间，页头取两者中较新的；这里留着是为了点完同步立刻显示「刚刚」。
 */
let lastManualSessionSyncAt: number | null = null;

/** 测试用：重置模块级的上次同步时间。 */
export function resetUsageSyncClockForTests() {
  lastManualSessionSyncAt = null;
}

/** 每 30 秒重渲染一次，让「N 分钟前同步」跟着走。 */
function useMinuteTicker() {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(timer);
  }, []);
  return now;
}

/** 首扫预告 / 同步进度的字节量：1024 进制，统一保留一位小数（52,428,800 → "50.0 MB"） */
function formatSyncBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 1024) return `${Math.max(0, bytes)} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = -1;
  do {
    value /= 1024;
    unit += 1;
  } while (value >= 1024 && unit < units.length - 1);
  return `${value.toFixed(1)} ${units[unit]}`;
}

const LOG_PAGE_SIZE_OPTIONS = LOG_PAGE_SIZES.map(Number);

const menuContentClass =
  "min-w-[200px] max-w-[320px] rounded-panel border-border bg-surface p-1 shadow-v7-md";
const menuItemClass = "h-[30px] gap-2 rounded-control px-2 text-body";

function MenuCheck({ checked }: { checked: boolean }) {
  return (
    <Check
      aria-hidden="true"
      strokeWidth={1.75}
      className={cn("h-3.5 w-3.5 shrink-0", !checked && "invisible")}
    />
  );
}

function MenuMeta({ children }: { children: React.ReactNode }) {
  return (
    <span className="ms-auto ps-3 text-caption tabular-nums text-fg-3">
      {children}
    </span>
  );
}

/** 级联菜单第二级：某个供应商（或全部供应商）名下的模型，展开时才查 */
function ScopeModelItems({
  range,
  appType,
  providerName,
  providerCount,
  active,
  selectedModel,
  locale,
  onPick,
}: {
  range: UsageRangeSelection;
  appType: AppTypeFilter;
  providerName: string | undefined;
  providerCount: number;
  /** 这一级就是当前选中的供应商（勾才画在它的模型上） */
  active: boolean;
  selectedModel: string | undefined;
  locale: string;
  onPick: (model: string | undefined) => void;
}) {
  const { t } = useTranslation();
  const { data } = useModelStats(
    range,
    { appType, providerName },
    { refetchInterval: false },
  );
  const options = useMemo(() => {
    const counts = new Map<string, number>();
    for (const stat of data ?? []) {
      counts.set(stat.model, (counts.get(stat.model) ?? 0) + stat.requestCount);
    }
    // 数据刷新后选中项可能掉出列表（如改了时间范围）；补回去保证看得见、能清除
    if (selectedModel && !counts.has(selectedModel)) {
      counts.set(selectedModel, 0);
    }
    return Array.from(counts, ([name, count]) => ({ name, count })).sort(
      (a, b) => b.count - a.count,
    );
  }, [data, selectedModel]);

  return (
    <>
      <DropdownMenuItem
        className={menuItemClass}
        onSelect={() => onPick(undefined)}
      >
        <MenuCheck checked={active && selectedModel == null} />
        {t("usage.allModels")}
        <MenuMeta>{fmtInt(providerCount, locale)}</MenuMeta>
      </DropdownMenuItem>
      {options.map((option) => (
        <DropdownMenuItem
          key={option.name}
          className={menuItemClass}
          title={option.name}
          onSelect={() => onPick(option.name)}
        >
          <MenuCheck checked={active && selectedModel === option.name} />
          <span className="min-w-0 truncate font-mono text-caption">
            {option.name}
          </span>
          <MenuMeta>{fmtInt(option.count, locale)}</MenuMeta>
        </DropdownMenuItem>
      ))}
    </>
  );
}

interface UsageDashboardProps {
  refreshIntervalMs?: number;
  onRefreshIntervalChange?: (next: number) => Promise<boolean> | boolean | void;
  sessionAutoSyncEnabled?: boolean;
  /** 设置还没读出来时为 false：此时 sessionAutoSyncEnabled 只是默认值，不能据此扫描 */
  settingsReady?: boolean;
  onSessionAutoSyncEnabledChange?: (
    next: boolean,
  ) => Promise<boolean> | boolean | void;
  /** 从应用页「查看此应用的用量」进入时带上的应用筛选 */
  initialAppType?: AppTypeFilter;
  /** 「数据来源」里的「修改记录请求用量」：打开设置 → 本地路由 */
  onOpenRoutingSettings?: () => void;
  /** 金额显示币种与汇率（设备级设置） */
  usageDisplay?: UsageDisplaySettings;
  onUsageDisplayChange?: (
    next: UsageDisplaySettings,
  ) => Promise<boolean> | boolean | void;
}

/**
 * 用量统计全局页：页头 → 范围栏 → 概览面板（指标条 + 趋势图 / 热力图）→ 明细面板（请求日志 / 供应商 / 模型）。
 */
export function UsageDashboard({
  refreshIntervalMs: savedRefreshIntervalMs,
  onRefreshIntervalChange,
  sessionAutoSyncEnabled = true,
  settingsReady = true,
  onSessionAutoSyncEnabledChange,
  initialAppType = "all",
  onOpenRoutingSettings,
  usageDisplay,
  onUsageDisplayChange,
}: UsageDashboardProps = {}) {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const [range, setRange] = useState<UsageRangeSelection>(() => ({
    preset: readRememberedChoice(
      USAGE_CHOICE_KEYS.range,
      REMEMBERED_RANGE_PRESETS,
      "today",
    ),
  }));
  const applyRange = (next: UsageRangeSelection) => {
    setRange(next);
    if (next.preset !== "custom") {
      writeRememberedChoice(USAGE_CHOICE_KEYS.range, next.preset);
    }
  };
  const [appType, setAppType] = useState<AppTypeFilter>(initialAppType);
  const [providerName, setProviderName] = useState<string | undefined>(
    undefined,
  );
  const [model, setModel] = useState<string | undefined>(undefined);
  const [statusCode, setStatusCode] = useState<number | undefined>(undefined);

  // 请求日志显示哪些列在「设置」页签里改，这里只按设置画
  const logColumns = resolveRequestLogColumns(usageDisplay?.requestLogColumns);
  const [tab, setTab] = useRememberedChoice<UsageTab>(
    USAGE_CHOICE_KEYS.tab,
    TABS,
    "logs",
  );
  // 指标条选中的指标，也是趋势图 / 热力图画的指标
  const [metric, setMetric] = useRememberedChoice<UsageMetric>(
    USAGE_CHOICE_KEYS.trendMetric,
    USAGE_METRICS,
    "tokens",
  );
  // 请求日志每页几条：记在本机；页签计数的查询也用它，和表格合并成一次请求
  const [logPageSizeChoice, setLogPageSizeChoice] =
    useRememberedChoice<LogPageSize>(
      USAGE_CHOICE_KEYS.logPageSize,
      LOG_PAGE_SIZES,
      "20",
    );
  const logPageSize = Number(logPageSizeChoice);
  // 定价对话框：抽屉里和空状态都能打开，新装的人往往先配好 models.dev 同步再开始用
  const [pricingOpen, setPricingOpen] = useState(false);
  const [refreshIntervalMs, setRefreshIntervalMs] = useState(() =>
    normalizeRefreshInterval(savedRefreshIntervalMs),
  );
  const [detailRequestId, setDetailRequestId] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [showRebuildConfirm, setShowRebuildConfirm] = useState(false);
  const [rebuildingCodex, setRebuildingCodex] = useState(false);
  const [syncingSession, setSyncingSession] = useState(false);
  // 重置该主机用量数据（危险操作）：入口在主机菜单底部，确认后才执行
  const [showResetConfirm, setShowResetConfirm] = useState(false);
  const [resetting, setResetting] = useState(false);
  const [lastManualSyncAt, setLastManualSyncAt] = useState(
    lastManualSessionSyncAt,
  );
  const { data: lastScanAt } = useSessionUsageLastSync({
    refetchInterval: false,
  });

  // ── 主机切换（SSH 远程迭代 2，设计 §4.4）──────────────────────────────
  // null = 本机；主机清单来自设置。选中主机被删时归一回本机（会话页同款），
  // 查询与菜单都吃归一值，避免界面显示「本机」、数据层还在请求旧主机
  const [hostId, setHostId] = useState<string | null>(null);
  const { data: settings } = useSettingsQuery();
  const remoteHostId = normalizeHostId(hostId, settings?.sshHosts);
  const isRemote = remoteHostId != null;

  // 远程同步运行态：首扫与手动增量共用（force 区分）；进度来自后端逐文件事件
  const [remoteSyncing, setRemoteSyncing] = useState(false);
  const [remoteProgress, setRemoteProgress] =
    useState<RemoteUsageSyncStatus | null>(null);
  const cancelRequested = useRef(false);

  const { syncSessionUsage } = useUsageAutoRefresh({
    refreshIntervalMs,
    sessionAutoSyncEnabled,
    paused: rebuildingCodex || !settingsReady,
    // 预设范围和「结束于现在」的自定义范围都随时间移动，没有新用量也要定时重算
    rollingRange: range.preset !== "custom" || !!range.liveEndTime,
    // 远程视图停扫本机会话，自动刷新改走查询内建的轻同步（清单 diff）
    remoteHostId,
  });
  // 远程视图的「上次同步」只认手动同步时间：本机后台扫描时间与远程无关
  const lastSyncAt = isRemote
    ? (lastManualSyncAt ?? null)
    : Math.max(lastManualSyncAt ?? 0, lastScanAt ?? 0) || null;
  const [containerRef, measuredWidth] = useElementWidth<HTMLDivElement>();
  const now = useMinuteTicker();

  // 测量不到（测试环境）按宽窗口排
  const width = measuredWidth || 1000;
  const compact = width < 720;

  useEffect(() => {
    setRefreshIntervalMs(normalizeRefreshInterval(savedRefreshIntervalMs));
  }, [savedRefreshIntervalMs]);

  useEffect(() => {
    setAppType(initialAppType);
    setProviderName(undefined);
    setModel(undefined);
  }, [initialAppType]);

  // 切应用时清掉下游筛选，避免留下一个在新范围内查无数据的"幽灵"组合；
  // 切供应商同理清掉模型（模型选项随供应商级联）。
  const changeAppType = (next: AppTypeFilter) => {
    setAppType(next);
    if (next !== appType) {
      setProviderName(undefined);
      setModel(undefined);
    }
  };
  // 供应商和模型一次选定（级联菜单）
  const [scopeMenuOpen, setScopeMenuOpen] = useState(false);
  const pickScope = (
    nextProvider: string | undefined,
    nextModel: string | undefined,
  ) => {
    setProviderName(nextProvider);
    setModel(nextModel);
    setScopeMenuOpen(false);
  };

  // 后端写入新日志时 emit `usage-log-recorded`，立刻 invalidate 所有 usage 查询
  useUsageEventBridge();

  // 远程同步进度：手动同步期间后端每处理完一个文件 emit 一次；
  // 轻同步（查询内建）不 emit，这里只在远程视图挂监听（先例 useSessionContentSearch）
  useEffect(() => {
    if (!isRemote) return;
    let unlisten: UnlistenFn | undefined;
    let disposed = false;
    (async () => {
      const off = await listen<RemoteUsageSyncStatus>(
        "remote-usage-sync-status",
        (event) => setRemoteProgress(event.payload),
      );
      if (disposed) {
        off();
      } else {
        unlisten = off;
      }
    })();
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [isRemote]);

  // 切主机时丢掉上一台主机的进度
  useEffect(() => {
    setRemoteProgress(null);
  }, [remoteHostId]);

  const changeRefreshInterval = async (next: number) => {
    const normalized = normalizeRefreshInterval(next);
    const previous = refreshIntervalMs;
    setRefreshIntervalMs(normalized);
    try {
      const saved = await onRefreshIntervalChange?.(normalized);
      if (saved === false) {
        setRefreshIntervalMs(previous);
      }
    } catch (error) {
      console.error(
        "[UsageDashboard] Failed to persist refresh interval",
        error,
      );
      setRefreshIntervalMs(previous);
    }
  };

  const rebuildCodexUsage = async () => {
    setShowRebuildConfirm(false);
    setRebuildingCodex(true);
    try {
      const result = await usageApi.rebuildCodexUsage();
      await queryClient.invalidateQueries({ queryKey: usageKeys.all });
      const message = t("usage.rebuildCodex.completed", {
        imported: result.imported,
        errors: result.errors.length,
        suspected: result.suspectedDuplicates,
        deferred: result.deferredFiles,
      });
      if (result.errors.length > 0 || result.deferredFiles > 0) {
        toast.warning(message);
      } else {
        toast.success(message);
      }
    } catch (error) {
      toast.error(t("usage.rebuildCodex.failed", { error: String(error) }));
    } finally {
      setRebuildingCodex(false);
    }
  };

  const runManualSessionSync = async () => {
    setSyncingSession(true);
    try {
      const result = await syncSessionUsage();
      await queryClient.invalidateQueries({ queryKey: usageKeys.all });
      lastManualSessionSyncAt = Date.now();
      setLastManualSyncAt(lastManualSessionSyncAt);
      const message = t("usage.sessionSync.syncCompleted", {
        imported: result.imported,
        files: result.filesScanned,
        errors: result.errors.length,
      });
      if (result.errors.length > 0) {
        toast.warning(message);
      } else {
        toast.success(message);
      }
    } catch (error) {
      toast.error(t("usage.sessionSync.syncFailed", { error: String(error) }));
    } finally {
      setSyncingSession(false);
    }
  };

  // ── 远程用量同步（首扫 force=true / 手动增量 force=false）─────────────
  // 取消或单文件失败都不毁整轮：命令以部分进度正常返回，失败才走 catch。
  // 结束后统一失效 usage 前缀——数据查询与 peek 预告一起刷新。
  const runRemoteSync = async (force: boolean) => {
    if (!remoteHostId || remoteSyncing) return;
    cancelRequested.current = false;
    setRemoteSyncing(true);
    setRemoteProgress(null);
    try {
      const result = await usageApi.syncRemoteUsage(remoteHostId, force);
      lastManualSessionSyncAt = Date.now();
      setLastManualSyncAt(lastManualSessionSyncAt);
      if (cancelRequested.current) {
        toast.info(t("usage.remoteSync.cancelled"));
      } else if (result.skipped > 0) {
        toast.warning(
          t("usage.remoteSync.completedSkipped", {
            files: result.doneFiles,
            skipped: result.skipped,
          }),
        );
      } else {
        toast.success(
          t("usage.remoteSync.completed", { files: result.doneFiles }),
        );
      }
    } catch (error) {
      toast.error(t("usage.remoteSync.failed", { error: String(error) }));
    } finally {
      setRemoteSyncing(false);
      setRemoteProgress(null);
      await queryClient.invalidateQueries({ queryKey: usageKeys.all });
    }
  };

  // ── 重置该主机用量数据（设计 §4.4）─────────────────────────────────────
  // 清空该主机本地镜像四表（明细 / 汇总 / 游标 / 去重），远端文件不受影响；
  // 成功后失效 usage 全系查询，页面自然回到首扫预告态（peek 重新统计）
  const resetTargetLabel =
    settings?.sshHosts?.find((host) => host.id === remoteHostId)?.label ??
    remoteHostId ??
    "";
  const resetRemoteData = async () => {
    if (!remoteHostId || resetting) return;
    setResetting(true);
    try {
      const summary = await usageApi.resetRemoteUsage(remoteHostId);
      await queryClient.invalidateQueries({ queryKey: usageKeys.all });
      toast.success(
        t("usage.hostSelector.resetEntry.successToast", {
          details: summary.deletedDetails,
          rollups: summary.deletedRollups,
        }),
      );
    } catch (error) {
      toast.error(
        t("usage.hostSelector.resetEntry.failed", { error: String(error) }),
      );
    } finally {
      setResetting(false);
      setShowResetConfirm(false);
    }
  };

  const language = i18n.resolvedLanguage || i18n.language || "en";
  const locale = getLocaleFromLanguage(language);
  const resolvedRange = useMemo(() => resolveUsageRange(range), [range]);
  const rangeLabel = useMemo(() => {
    if (range.preset !== "custom") {
      return getUsageRangePresetLabel(range.preset, t);
    }
    const startStr = new Date(resolvedRange.startDate * 1000).toLocaleString(
      locale,
    );
    if (range.liveEndTime) {
      return `${startStr} → ${t("usage.liveEndTimeNow", "现在")}`;
    }
    const endStr = new Date(resolvedRange.endDate * 1000).toLocaleString(
      locale,
    );
    return `${startStr} - ${endStr}`;
  }, [locale, range, resolvedRange.endDate, resolvedRange.startDate, t]);

  // 筛选下拉的选项池：供应商列表只跟应用/时间范围走（不受自身选中值影响），
  // 模型列表随所选供应商级联。所有查询（含子组件）都不轮询：会话用量由
  // useUsageAutoRefresh 按间隔导入，导入或路由写入后由 useUsageEventBridge 刷新。
  const refetch = {
    refetchInterval: false as const,
  };
  const { data: providerOptionsData } = useProviderStats(
    range,
    { appType },
    { ...refetch, hostId: remoteHostId },
  );
  // 有没有任何用量（不分时间范围）：一条都没有时显示空状态；远程视图同时
  // 兼任链路探针——带 hostId 的这条查询失败即远程加载失败（错误条 + 重试）
  const { data: allTimeSummary, error: allTimeError } = useQuery({
    queryKey: [...usageKeys.all, remoteHostId ?? "local", "all-time-summary"],
    queryFn: () =>
      usageApi.getUsageSummary(
        undefined,
        undefined,
        undefined,
        undefined,
        undefined,
        remoteHostId ?? undefined,
      ),
    retry: false,
    refetchInterval: refetch.refetchInterval,
  });
  const isEmpty = allTimeSummary != null && allTimeSummary.totalRequests === 0;

  // 首扫预告：清单 diff 单往返统计待同步文件数 / 待拉总字节（不拉取不写库）。
  // 失败静默（无预告即正常展示数据），链路错误由上面的汇总探针统一报
  const { data: remotePeek } = useQuery({
    queryKey: usageKeys.remotePeek(remoteHostId ?? "local"),
    queryFn: () => usageApi.peekRemoteUsage(remoteHostId!),
    enabled: isRemote && !remoteSyncing,
    retry: false,
    refetchInterval: false,
  });
  // 无任何已入库数据（从未建过游标）且确有待同步文件 → 首扫确认态
  const needsFirstSync =
    isRemote && isEmpty && (remotePeek?.totalFiles ?? 0) > 0;
  const remoteLoadFailed = isRemote && allTimeError != null;

  const providerOptions = useMemo(() => {
    const counts = new Map<string, number>();
    for (const stat of providerOptionsData ?? []) {
      counts.set(
        stat.providerName,
        (counts.get(stat.providerName) ?? 0) + stat.requestCount,
      );
    }
    // 数据刷新后选中项可能掉出列表（如改了时间范围）；补回去保证用户看得见、能清除
    if (providerName && !counts.has(providerName)) counts.set(providerName, 0);
    return Array.from(counts, ([name, count]) => ({ name, count })).sort(
      (a, b) => b.count - a.count,
    );
  }, [providerOptionsData, providerName]);

  const providerTotal = providerOptions.reduce((sum, p) => sum + p.count, 0);
  // 触发器：「供应商 / 模型」，哪一级选了就换成选中的名字
  const providerText = providerName
    ? getUsageProviderLabel(providerName, t).label
    : t("usage.providerFilter.label");
  const scopeLabel = `${providerText} / ${model ?? t("usage.modelFilter.label")}`;
  const scopeTitle = `${
    providerName
      ? usageProviderTitle(getUsageProviderLabel(providerName, t))
      : t("usage.providerFilter.all")
  } / ${model ?? t("usage.allModels")}`;

  // ── 页头 ─────────────────────────────────────────────────────────────
  // 同步按钮只说一件事：最近一次同步是什么时候；自动扫描关着时悬停提示里说明
  const syncedLabel = useMemo(() => {
    if (lastSyncAt == null) return undefined;
    const minutes = Math.max(0, Math.floor((now - lastSyncAt) / 60_000));
    return minutes < 1
      ? t("usage.syncStatus.justNow")
      : t("usage.syncStatus.minutesAgo", { count: minutes });
  }, [lastSyncAt, now, t]);
  // 页头同步按钮：本机扫会话日志；远程走轻量增量（force=false）。
  // 两态互斥禁用，标签共用「同步中 / 刚刚同步 / 立即同步」
  const manualSyncBusy = syncingSession || remoteSyncing;
  const syncButtonLabel = manualSyncBusy
    ? t("usage.syncStatus.syncing")
    : (syncedLabel ?? t("usage.sessionSync.syncNow"));
  const drawerSyncedLabel =
    lastSyncAt == null
      ? undefined
      : t("usage.sources.lastSynced", {
          time: formatRelativeTime(lastSyncAt, t, now),
        });

  const headerActions = (
    <div className="flex h-8 shrink-0 items-stretch overflow-hidden rounded-control border border-border-strong bg-surface">
      <HoverTip
        content={
          sessionAutoSyncEnabled
            ? t("usage.syncStatus.tip")
            : t("usage.syncStatus.tipOff")
        }
      >
        <button
          type="button"
          className="inline-flex items-center gap-1.5 whitespace-nowrap pe-3 ps-2.5 text-body font-medium tabular-nums text-fg-1 transition-colors hover:bg-subtle focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring disabled:text-fg-2"
          disabled={manualSyncBusy}
          onClick={() =>
            void (isRemote ? runRemoteSync(false) : runManualSessionSync())
          }
        >
          <RefreshCw
            aria-hidden="true"
            className={cn(
              "h-3.5 w-3.5 text-fg-2",
              manualSyncBusy && "motion-safe:animate-spin",
            )}
          />
          {syncButtonLabel}
        </button>
      </HoverTip>
      <HoverTip content={t("usage.settings.title")}>
        <button
          type="button"
          aria-label={t("usage.settings.title")}
          aria-haspopup="dialog"
          className="inline-flex w-8 items-center justify-center border-s border-border text-fg-2 transition-colors hover:bg-subtle hover:text-fg-1 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring"
          onClick={() => setSettingsOpen(true)}
        >
          <Settings2 aria-hidden="true" className="h-3.5 w-3.5" />
        </button>
      </HoverTip>
    </div>
  );

  // ── 筛选行 ───────────────────────────────────────────────────────────
  // 选中块是单独的滑块（SegmentThumb），切换应用时滑过去
  // 只列「应用」页设为显示的应用；Claude Code 的用量在 Claude Desktop 显示时也算，
  // DeepSeek Harness（Web 与 Desktop 的会话用量合并记在 dsh 下）在 Desktop 显示时也算，
  // 当前选中的即使被隐藏也留着，免得筛选状态无处可见
  const filterApps = useMemo(() => {
    const visible = { ...DEFAULT_VISIBLE_APPS, ...settings?.visibleApps };
    return KNOWN_APP_TYPES.filter(
      (app) =>
        visible[app] ||
        (app === "claude" && visible["claude-desktop"]) ||
        app === appType,
    );
  }, [settings?.visibleApps, appType]);
  const appFilterIndicator = useSlidingIndicator<HTMLDivElement>(
    '[aria-pressed="true"]',
    appType,
  );
  const chipClass = (pressed: boolean) =>
    cn(
      "relative inline-flex h-[26px] shrink-0 items-center gap-1.5 whitespace-nowrap rounded-[5px] px-2 text-body transition-colors duration-150",
      pressed
        ? "font-semibold text-fg-1"
        : "font-medium text-fg-2 hover:text-fg-1",
    );

  // 远程同步区（filterRow 内、主机菜单旁）：同步中进度 / 首扫预告 / 加载失败，
  // 三态互斥，本机视图不渲染任何东西
  const remoteSyncBar = !isRemote ? null : remoteSyncing ? (
    <div className="flex h-8 shrink-0 items-center gap-2 rounded-control border border-border-strong bg-surface px-2.5">
      <Loader2
        aria-hidden="true"
        className="h-3.5 w-3.5 shrink-0 animate-spin text-fg-2"
      />
      {remoteProgress ? (
        <span className="whitespace-nowrap text-caption tabular-nums text-fg-2">
          {t("usage.remoteSync.progress", {
            done: remoteProgress.doneFiles,
            total: remoteProgress.totalFiles,
            bytes: formatSyncBytes(remoteProgress.doneBytes),
          })}
        </span>
      ) : (
        <span className="whitespace-nowrap text-caption text-fg-2">
          {t("usage.syncStatus.syncing")}
        </span>
      )}
      <Button
        type="button"
        variant="quiet"
        size="compact"
        className="text-caption"
        onClick={() => {
          // 请求取消：引擎在下一个文件边界停止，已入库数据与游标保留
          cancelRequested.current = true;
          void usageApi.cancelRemoteUsageSync();
        }}
      >
        {t("usage.remoteSync.cancel")}
      </Button>
    </div>
  ) : needsFirstSync ? (
    <div className="flex h-8 shrink-0 items-center gap-2 rounded-control border border-border-strong bg-surface px-2.5">
      <span className="whitespace-nowrap text-caption text-fg-2">
        {t("usage.remoteSync.firstSyncPreview", {
          files: remotePeek!.totalFiles,
          size: formatSyncBytes(remotePeek!.doneBytes),
        })}
      </span>
      <Button
        type="button"
        variant="neutral"
        size="compact"
        className="text-caption"
        onClick={() => void runRemoteSync(true)}
      >
        {t("usage.remoteSync.start")}
      </Button>
    </div>
  ) : remoteLoadFailed ? (
    <div className="flex h-8 min-w-0 shrink-0 items-center gap-2 rounded-control border border-border-strong bg-surface px-2.5">
      <TriangleAlert
        aria-hidden="true"
        className="h-3.5 w-3.5 shrink-0 text-destructive"
      />
      <span
        className="min-w-0 truncate text-caption text-fg-2"
        title={String(allTimeError)}
      >
        {t("usage.remoteSync.loadFailed", {
          error: String(allTimeError),
        })}
      </span>
      <Button
        type="button"
        variant="quiet"
        size="compact"
        className="shrink-0 text-caption"
        onClick={() =>
          void queryClient.invalidateQueries({
            queryKey: usageKeys.all,
          })
        }
      >
        {t("usage.remoteSync.retry")}
      </Button>
    </div>
  ) : null;

  // 范围栏：决定下面所有数据的四个条件放一组、左对齐；时间排第一，图表和指标都跟着它
  const filterRow = (
    <div className="flex min-h-12 shrink-0 flex-wrap items-center gap-2 px-6 py-2">
      <UsageDateRangePicker
        selection={range}
        triggerLabel={rangeLabel}
        onApply={applyRange}
      />
      <span aria-hidden="true" className="mx-1 h-4 w-px bg-border" />
      {/* 主机切换：本机 / 远程 SSH 主机；没配主机时整块不渲染。
          悬停说明走 triggerHint 由组件自己挂：菜单展开期间自动收起，不遮挡列表；
          重置入口随菜单走（本机态组件内部不渲染），不占常驻空间 */}
      {(settings?.sshHosts?.length ?? 0) > 0 && (
        <SessionHostMenu
          hostId={hostId}
          sshHosts={settings?.sshHosts}
          onSelect={setHostId}
          triggerHint={t("usage.hostSelector.hint")}
          dangerEntry={{
            label: t("usage.hostSelector.resetEntry.entry"),
            onTrigger: () => setShowResetConfirm(true),
            // 同步进行中禁用：重置清表与 sync 的增量写入并发会互相打架，
            // 等 sync 收尾（进度条消失）再重置才能干净回到首扫预告态
            disabled: remoteSyncing,
          }}
        />
      )}
      {remoteSyncBar}
      <div
        ref={appFilterIndicator.ref}
        role="group"
        aria-label={t("usage.appFilter.label")}
        className="relative flex h-8 shrink-0 items-center gap-0.5 rounded-[8px] border border-border-strong bg-surface p-[2px]"
      >
        <SegmentThumb
          rect={appFilterIndicator.rect}
          animate={appFilterIndicator.animate}
          // 和旁边的按钮同一套（卡片底 + 描边），选中项是按钮里凹下去的一块；
          // 不用共用的 bg-subtle 底槽：暗色下它比页面亮、比按钮暗，成了第三种亮度
          className="rounded-[5px] bg-subtle shadow-none"
        />
        <button
          type="button"
          aria-pressed={appType === "all"}
          className={chipClass(appType === "all")}
          onClick={() => changeAppType("all")}
        >
          {t("usage.appFilter.all")}
        </button>
        {/* 应用只露图标，名字放悬停提示和 aria-label */}
        {filterApps.map((app) => (
          <HoverTip key={app} content={APP_DISPLAY_NAME[app]}>
            <button
              type="button"
              aria-pressed={appType === app}
              aria-label={APP_DISPLAY_NAME[app]}
              className={cn(
                chipClass(appType === app),
                "w-[30px] justify-center px-0",
              )}
              onClick={() => changeAppType(app)}
            >
              <AppGlyph
                app={app}
                size={16}
                badgeClassName={appType === app ? "bg-subtle" : "bg-surface"}
              />
            </button>
          </HoverTip>
        ))}
      </div>
      {/* 供应商 / 模型是上下级：级联菜单，悬停供应商展开它名下的模型；
          点供应商本身＝只按供应商筛（全部模型） */}
      <DropdownMenu open={scopeMenuOpen} onOpenChange={setScopeMenuOpen}>
        <DropdownMenuTrigger asChild>
          <Button
            type="button"
            variant="neutral"
            size="regular"
            className="min-w-0 max-w-[260px] shrink gap-1.5 pe-2 ps-2.5"
            title={scopeTitle}
          >
            <ListTree
              aria-hidden="true"
              className="h-4 w-4 shrink-0 text-fg-3"
            />
            <span className="min-w-0 truncate">{scopeLabel}</span>
            <ChevronDown className="h-3.5 w-3.5 shrink-0 text-fg-2" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent
          align="start"
          aria-label={t("usage.providerFilter.title")}
          className={cn(menuContentClass, "max-h-[360px] overflow-y-auto")}
        >
          {[{ name: undefined, count: providerTotal }, ...providerOptions].map(
            (option) => {
              const label =
                option.name == null
                  ? null
                  : getUsageProviderLabel(option.name, t);
              const active = providerName === option.name;
              return (
                <DropdownMenuSub key={option.name ?? ""}>
                  <DropdownMenuSubTrigger
                    className={menuItemClass}
                    title={label ? usageProviderTitle(label) : undefined}
                    onClick={() => pickScope(option.name, undefined)}
                  >
                    <MenuCheck checked={active} />
                    <span className="min-w-0 truncate">
                      {label ? label.label : t("usage.providerFilter.all")}
                    </span>
                    <MenuMeta>{fmtInt(option.count, locale)}</MenuMeta>
                    <ChevronRight
                      aria-hidden="true"
                      className="-me-0.5 h-3.5 w-3.5 shrink-0 text-fg-3"
                    />
                  </DropdownMenuSubTrigger>
                  <DropdownMenuPortal>
                    <DropdownMenuSubContent
                      // 子菜单默认顶边对齐触发项；减去自身 1px 边框 + 4px 内边距，第一项才和触发项同一行
                      alignOffset={-5}
                      aria-label={t("usage.modelFilter.title")}
                      className={cn(
                        menuContentClass,
                        "max-h-[360px] overflow-y-auto",
                      )}
                    >
                      <ScopeModelItems
                        range={range}
                        appType={appType}
                        providerName={option.name}
                        providerCount={option.count}
                        active={active}
                        selectedModel={active ? model : undefined}
                        locale={locale}
                        onPick={(next) => pickScope(option.name, next)}
                      />
                    </DropdownMenuSubContent>
                  </DropdownMenuPortal>
                </DropdownMenuSub>
              );
            },
          )}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );

  // ── 明细面板 ─────────────────────────────────────────────────────────
  const scopedAppType = appType === "all" ? undefined : appType;

  // 页签上的计数：和各表格同样的查询参数，react-query 会合并成一次请求。
  // 三个页签都查（只查当前页签时，没点过的页签一直没有数字）；换筛选时沿用上一份
  // 数据直到新数到达，三个数字原地更新，不会先整体消失再冒出来把页签栏挤得抖一下
  const logCount = useRequestLogs({
    filters: { appType: scopedAppType, providerName, model, statusCode },
    range,
    page: 0,
    pageSize: logPageSize,
    options: { ...refetch, hostId: remoteHostId },
  });
  const providerCount = useProviderStats(
    range,
    { appType, providerName, model },
    { ...refetch, hostId: remoteHostId },
  );
  const modelCount = useModelStats(
    range,
    { appType, providerName, model },
    { ...refetch, hostId: remoteHostId },
  );
  const tabCount: Record<UsageTab, number | undefined> = {
    logs: logCount.data?.total,
    providers: providerCount.data?.length,
    models: modelCount.data?.length,
  };
  const tabLabel: Record<UsageTab, string> = {
    logs: t("usage.requestLogs"),
    providers: t("usage.tabs.providers"),
    models: t("usage.tabs.models"),
  };
  const statusLabel =
    statusCode == null
      ? t("usage.statusFilter.all")
      : `${t("usage.statusCode")} ${statusCode}`;

  // 工具栏两类入口：左边「筛选」（漏斗图标 + 当前条件，生效时实心底）改的是看哪些行；
  // 竖线右边「显示的列」（纯图标）改的是表怎么显示，是存下来的偏好
  const tabTrailing =
    tab === "logs" ? (
      <div className="flex items-center gap-2">
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              type="button"
              variant="quiet"
              size="compact"
              className={cn(
                "gap-1.5 pe-1.5 ps-2 text-caption",
                statusCode == null
                  ? "text-fg-2"
                  : "bg-subtle font-medium text-fg-1",
              )}
              aria-label={`${t("usage.statusCode")}: ${statusLabel}`}
            >
              <ListFilter aria-hidden="true" className="h-3.5 w-3.5" />
              {statusCode == null
                ? t("usage.statusCode")
                : `${t("usage.statusCode")} · ${statusCode}`}
              <ChevronDown aria-hidden="true" className="h-3.5 w-3.5" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent
            align="end"
            className={cn(menuContentClass, "min-w-[160px]")}
          >
            <DropdownMenuItem
              className={menuItemClass}
              onSelect={() => setStatusCode(undefined)}
            >
              <MenuCheck checked={statusCode == null} />
              {t("usage.statusFilter.all")}
            </DropdownMenuItem>
            {STATUS_CODE_OPTIONS.map((code) => (
              <DropdownMenuItem
                key={code}
                className={menuItemClass}
                onSelect={() => setStatusCode(code)}
              >
                <MenuCheck checked={statusCode === code} />
                <span className="tabular-nums">{code}</span>
              </DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>
        {onUsageDisplayChange && (
          <>
            <span aria-hidden="true" className="h-4 w-px bg-border" />
            <RequestLogColumnsMenu
              value={usageDisplay}
              onChange={onUsageDisplayChange}
            />
          </>
        )}
      </div>
    ) : null;

  const detailPanel = (
    <section
      aria-label={t("usage.tabs.label")}
      className="flex shrink-0 flex-col overflow-hidden rounded-panel border border-border bg-surface"
    >
      <PageTabs<UsageTab>
        aria-label={t("usage.tabs.label")}
        className="h-11 pe-2 ps-4"
        items={TABS.map((id) => ({
          value: id,
          label: (
            <span className="inline-flex items-center gap-1.5">
              {tabLabel[id]}
              {tabCount[id] != null && (
                <span className="rounded-[4px] bg-subtle px-1.5 text-caption font-medium tabular-nums text-fg-3">
                  {fmtInt(tabCount[id], locale)}
                </span>
              )}
            </span>
          ),
        }))}
        value={tab}
        onValueChange={setTab}
        idPrefix="usage-tab"
        controls="usage-tabpanel"
        trailing={tabTrailing}
      />
      <div
        role="tabpanel"
        id="usage-tabpanel"
        aria-labelledby={`usage-tab-${tab}`}
      >
        {tab === "logs" && (
          <RequestLogTable
            range={range}
            appType={appType}
            providerName={providerName}
            model={model}
            statusCode={statusCode}
            refreshIntervalMs={0}
            onOpenDetail={setDetailRequestId}
            columns={logColumns}
            pageSize={logPageSize}
            pageSizeOptions={LOG_PAGE_SIZE_OPTIONS}
            onPageSizeChange={(size) =>
              setLogPageSizeChoice(String(size) as LogPageSize)
            }
          />
        )}
        {tab === "providers" && (
          <ProviderStatsTable
            range={range}
            appType={appType}
            providerName={providerName}
            model={model}
            refreshIntervalMs={0}
          />
        )}
        {tab === "models" && (
          <ModelStatsTable
            range={range}
            appType={appType}
            providerName={providerName}
            model={model}
            refreshIntervalMs={0}
          />
        )}
      </div>
    </section>
  );

  // 概览面板：指标条和图表是同一块面，点指标换图表画的指标
  const overviewPanel = (
    <section className="shrink-0 overflow-hidden rounded-panel border border-border bg-surface">
      <UsageHero
        range={range}
        appType={scopedAppType}
        providerName={providerName}
        model={model}
        refreshIntervalMs={0}
        compact={compact}
        metric={metric}
        onMetricChange={setMetric}
      />
      {/* 「全部」看长期分布用热力图；24 小时到 30 天这类短范围用趋势图 */}
      {range.preset === "all" ? (
        <UsageHeatmap
          metric={metric}
          appType={appType}
          providerName={providerName}
          model={model}
          refreshIntervalMs={0}
        />
      ) : (
        <UsageTrendChart
          range={range}
          metric={metric}
          appType={appType}
          providerName={providerName}
          model={model}
          refreshIntervalMs={0}
        />
      )}
    </section>
  );

  const body = isEmpty ? (
    <div className="flex min-h-0 flex-1 flex-col items-center overflow-y-auto scroll-stable px-6 pb-6">
      <div className="mt-16 flex max-w-[440px] flex-col items-center gap-3 text-center">
        <span
          aria-hidden="true"
          className="flex h-11 w-11 items-center justify-center rounded-full bg-subtle text-fg-2"
        >
          <ChartColumn className="h-5 w-5" strokeWidth={1.5} />
        </span>
        <div className="flex flex-col gap-1">
          <p className="m-0 text-section text-fg-1">{t("usage.empty.title")}</p>
          <p className="m-0 text-body text-fg-2">{t("usage.empty.body")}</p>
        </div>
        {/* 远程视图的首扫入口集中在 filterRow 的预告条（含文件数 / 流量预告），
            这里不再放一个语义不明的「立即同步」 */}
        {!isRemote && (
          <Button
            type="button"
            variant="neutral"
            size="regular"
            disabled={syncingSession}
            onClick={() => void runManualSessionSync()}
          >
            {syncingSession && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
            {t("usage.sessionSync.syncNow")}
          </Button>
        )}
        <Button
          type="button"
          variant="quiet"
          size="regular"
          onClick={() => setPricingOpen(true)}
        >
          {t("usage.empty.configurePricing")}
        </Button>
      </div>
    </div>
  ) : (
    <div
      id="main-content"
      className="flex min-h-0 flex-1 flex-col gap-8 overflow-y-auto scroll-stable px-6 pb-6 pt-1"
    >
      {overviewPanel}
      {detailPanel}
    </div>
  );

  return (
    // 当前统计主机下发给子树：Hero / 图表 / 表格的查询 hooks 从 context 取
    // hostId，组件自身不逐层透传（页面本体的查询已显式传过）
    <UsageHostProvider value={remoteHostId}>
      <div ref={containerRef} className="flex min-h-0 min-w-0 flex-1 flex-col">
        <AppPageHeader
          icon={<ChartColumn className="h-5 w-5" strokeWidth={1.5} />}
          title={t("nav.usage")}
          titleExtra={
            <HelpTip title={t("nav.usage")}>{t("usage.subtitle")}</HelpTip>
          }
          actions={headerActions}
        />
        {filterRow}
        {body}

        <RequestDetailPanel
          requestId={detailRequestId}
          onClose={() => setDetailRequestId(null)}
        />
        <UsageSettingsSheet
          open={settingsOpen}
          onOpenChange={setSettingsOpen}
          sessionAutoSyncEnabled={sessionAutoSyncEnabled}
          onSessionAutoSyncEnabledChange={(value) =>
            void onSessionAutoSyncEnabledChange?.(value)
          }
          syncedLabel={drawerSyncedLabel}
          syncing={syncingSession}
          onSyncNow={() => void runManualSessionSync()}
          onOpenRoutingSettings={onOpenRoutingSettings}
          rebuildingCodex={rebuildingCodex}
          onRebuildCodex={() => setShowRebuildConfirm(true)}
          refreshIntervalMs={refreshIntervalMs}
          refreshIntervalOptions={REFRESH_INTERVAL_OPTIONS_MS}
          onRefreshIntervalChange={(next) => void changeRefreshInterval(next)}
          usageDisplay={usageDisplay}
          onUsageDisplayChange={onUsageDisplayChange}
          onOpenPricing={() => setPricingOpen(true)}
        />
        {/* 定价是二级页不是模态框：新增 / 编辑定价的编辑页也是 FullScreenPanel，
          放进模态框会被压在遮罩下、点不到 */}
        <FullScreenPanel
          isOpen={pricingOpen}
          title={t("usage.tabs.pricing")}
          onClose={() => setPricingOpen(false)}
        >
          <PricingConfigPanel />
        </FullScreenPanel>
        <ConfirmDialog
          isOpen={showRebuildConfirm}
          title={t("usage.rebuildCodex.confirmTitle")}
          message={t("usage.rebuildCodex.confirmMessage")}
          confirmText={t("usage.rebuildCodex.confirmAction")}
          // 重建前会自动备份数据库，可从备份恢复：不算不可撤销，不用红色确认键
          variant="info"
          zIndex="top"
          onConfirm={() => void rebuildCodexUsage()}
          onCancel={() => setShowRebuildConfirm(false)}
        />
        {/* 重置该主机用量数据：本地镜像清空不可撤销，走红色危险确认 */}
        <ConfirmDialog
          isOpen={showResetConfirm}
          title={t("usage.hostSelector.resetEntry.confirmTitle", {
            label: resetTargetLabel,
          })}
          message={t("usage.hostSelector.resetEntry.confirmBody")}
          confirmText={t("usage.hostSelector.resetEntry.confirmAction")}
          pending={resetting}
          onConfirm={() => void resetRemoteData()}
          onCancel={() => setShowResetConfirm(false)}
        />
      </div>
    </UsageHostProvider>
  );
}
