import { createContext, useContext } from "react";
import {
  keepPreviousData,
  useQuery,
  useMutation,
  useQueryClient,
} from "@tanstack/react-query";
import { usageApi } from "@/lib/api/usage";
import { resolveUsageRange } from "@/lib/usageRange";
import type {
  LogFilters,
  UsageRangeSelection,
  UsageScopeFilters,
} from "@/types/usage";

const DEFAULT_REFETCH_INTERVAL_MS = 30000;

/* ===== 主机作用域（SSH 远程迭代 2，设计 §4.4）===== */

/**
 * 当前统计主机（null = 本机）。UsageDashboard 挂 Provider；Hero / 图表 /
 * 表格子组件的 hooks 不接 hostId prop，统一从这里取。页面外的调用
 * （侧栏今日摘要等）没有 Provider，落到 null = 本机，行为与原先一致。
 */
const UsageHostContext = createContext<string | null>(null);

export const UsageHostProvider = UsageHostContext.Provider;

const useUsageHostId = () => useContext(UsageHostContext);

/** query key 里的主机占位：本机用 "local"（对齐 sessionKeys.list 先例） */
const hostKey = (hostId: string | null | undefined) => hostId ?? "local";

/** invoke 参数里的主机：null/undefined 归一成 undefined，字段不进请求 */
const hostArg = (hostId: string | null | undefined) => hostId ?? undefined;

type UsageQueryOptions = {
  enabled?: boolean;
  refetchInterval?: number | false;
  refetchIntervalInBackground?: boolean;
  /** 显式指定统计主机，覆盖 UsageHostProvider 的当前值（页面本体的调用用） */
  hostId?: string | null;
};

type RequestLogsQueryArgs = {
  filters: LogFilters;
  range: UsageRangeSelection;
  page?: number;
  pageSize?: number;
  options?: UsageQueryOptions;
};

type RequestLogsKey = {
  preset: UsageRangeSelection["preset"];
  customStartDate?: number;
  customEndDate?: number;
  liveEndTime?: boolean;
  appType?: string;
  providerName?: string;
  model?: string;
  statusCode?: number;
  projectDir?: string;
};

// Query keys
// 数据类 key 的第二维是主机：本机 "local" 占位、远程用主机 id，切换主机
// 缓存不串；detail / pricing / limits / script 等与主机无关的 key 不带这一维。
export const usageKeys = {
  all: ["usage"] as const,
  summary: (
    hostId: string | null,
    preset: UsageRangeSelection["preset"],
    customStartDate: number | undefined,
    customEndDate: number | undefined,
    filters?: UsageScopeFilters,
    liveEndTime?: boolean,
  ) =>
    [
      ...usageKeys.all,
      hostKey(hostId),
      "summary",
      preset,
      customStartDate ?? 0,
      customEndDate ?? 0,
      liveEndTime ?? false,
      filters?.appType ?? null,
      filters?.providerName ?? null,
      filters?.model ?? null,
    ] as const,
  summaryByApp: (
    hostId: string | null,
    preset: UsageRangeSelection["preset"],
    customStartDate: number | undefined,
    customEndDate: number | undefined,
    filters?: Pick<UsageScopeFilters, "providerName" | "model">,
    liveEndTime?: boolean,
  ) =>
    [
      ...usageKeys.all,
      hostKey(hostId),
      "summary-by-app",
      preset,
      customStartDate ?? 0,
      customEndDate ?? 0,
      liveEndTime ?? false,
      filters?.providerName ?? null,
      filters?.model ?? null,
    ] as const,
  trends: (
    hostId: string | null,
    preset: UsageRangeSelection["preset"],
    customStartDate: number | undefined,
    customEndDate: number | undefined,
    filters?: UsageScopeFilters,
    liveEndTime?: boolean,
  ) =>
    [
      ...usageKeys.all,
      hostKey(hostId),
      "trends",
      preset,
      customStartDate ?? 0,
      customEndDate ?? 0,
      liveEndTime ?? false,
      filters?.appType ?? null,
      filters?.providerName ?? null,
      filters?.model ?? null,
    ] as const,
  firstDate: (hostId: string | null, filters?: UsageScopeFilters) =>
    [
      ...usageKeys.all,
      hostKey(hostId),
      "first-date",
      filters?.appType ?? null,
      filters?.providerName ?? null,
      filters?.model ?? null,
    ] as const,
  providerStats: (
    hostId: string | null,
    preset: UsageRangeSelection["preset"],
    customStartDate: number | undefined,
    customEndDate: number | undefined,
    filters?: UsageScopeFilters,
    liveEndTime?: boolean,
  ) =>
    [
      ...usageKeys.all,
      hostKey(hostId),
      "provider-stats",
      preset,
      customStartDate ?? 0,
      customEndDate ?? 0,
      liveEndTime ?? false,
      filters?.appType ?? null,
      filters?.providerName ?? null,
      filters?.model ?? null,
    ] as const,
  modelStats: (
    hostId: string | null,
    preset: UsageRangeSelection["preset"],
    customStartDate: number | undefined,
    customEndDate: number | undefined,
    filters?: UsageScopeFilters,
    liveEndTime?: boolean,
  ) =>
    [
      ...usageKeys.all,
      hostKey(hostId),
      "model-stats",
      preset,
      customStartDate ?? 0,
      customEndDate ?? 0,
      liveEndTime ?? false,
      filters?.appType ?? null,
      filters?.providerName ?? null,
      filters?.model ?? null,
    ] as const,
  logs: (
    hostId: string | null,
    key: RequestLogsKey,
    page: number,
    pageSize: number,
  ) =>
    [
      ...usageKeys.all,
      hostKey(hostId),
      "logs",
      key.preset,
      key.customStartDate ?? 0,
      key.customEndDate ?? 0,
      key.liveEndTime ?? false,
      key.appType ?? "",
      key.providerName ?? "",
      key.model ?? "",
      key.statusCode ?? -1,
      key.projectDir ?? "",
      page,
      pageSize,
    ] as const,
  /** 远程首扫预告（peek）：待同步文件数 / 待拉总字节，随 usage 前缀一起失效 */
  remotePeek: (hostId: string) =>
    [...usageKeys.all, hostKey(hostId), "remote-peek"] as const,
  detail: (requestId: string) =>
    [...usageKeys.all, "detail", requestId] as const,
  pricing: () => [...usageKeys.all, "pricing"] as const,
  session: (appType: string, sessionId: string) =>
    [...usageKeys.all, "session", appType, sessionId] as const,
  limits: (providerId: string, appType: string) =>
    [...usageKeys.all, "limits", providerId, appType] as const,
  script: (providerId: string, appType: string) =>
    [...usageKeys.all, providerId, appType] as const,
};

/** 把 UI 侧的 "all" 哨兵归一成 undefined（后端语义：不过滤）。 */
function normalizeScopeFilters(filters?: UsageScopeFilters): UsageScopeFilters {
  return {
    appType: filters?.appType === "all" ? undefined : filters?.appType,
    providerName: filters?.providerName,
    model: filters?.model,
  };
}

/**
 * hooks 内部共用：统计主机 = 显式 options.hostId ?? Provider 当前值。
 * 页面本体的查询（UsageDashboard 组件体内）在自身 Provider 之外，
 * 必须显式传；子组件走 Provider。
 */
function useResolvedHostId(options?: UsageQueryOptions) {
  const contextHost = useUsageHostId();
  return options?.hostId !== undefined ? options.hostId : contextHost;
}

// Hooks
// 统计类查询都带 keepPreviousData：换筛选、时间范围、翻页时先留着上一份数据，
// 新数据到了再换，不让指标闪成「…」、请求日志表塌成骨架。
export function useUsageSummary(
  range: UsageRangeSelection,
  filters?: UsageScopeFilters,
  options?: UsageQueryOptions,
) {
  const effective = normalizeScopeFilters(filters);
  const hostId = useResolvedHostId(options);
  return useQuery({
    queryKey: usageKeys.summary(
      hostId,
      range.preset,
      range.customStartDate,
      range.customEndDate,
      effective,
      range.liveEndTime,
    ),
    queryFn: () => {
      const { startDate, endDate } = resolveUsageRange(range);
      return usageApi.getUsageSummary(
        startDate,
        endDate,
        effective.appType,
        effective.providerName,
        effective.model,
        hostArg(hostId),
      );
    },
    placeholderData: keepPreviousData,
    refetchInterval: options?.refetchInterval ?? DEFAULT_REFETCH_INTERVAL_MS,
    refetchIntervalInBackground: options?.refetchIntervalInBackground ?? false,
  });
}

export function useUsageSummaryByApp(
  range: UsageRangeSelection,
  filters?: Pick<UsageScopeFilters, "providerName" | "model">,
  options?: UsageQueryOptions,
) {
  const hostId = useResolvedHostId(options);
  return useQuery({
    queryKey: usageKeys.summaryByApp(
      hostId,
      range.preset,
      range.customStartDate,
      range.customEndDate,
      filters,
      range.liveEndTime,
    ),
    queryFn: () => {
      const { startDate, endDate } = resolveUsageRange(range);
      return usageApi.getUsageSummaryByApp(
        startDate,
        endDate,
        filters?.providerName,
        filters?.model,
        hostArg(hostId),
      );
    },
    placeholderData: keepPreviousData,
    refetchInterval: options?.refetchInterval ?? DEFAULT_REFETCH_INTERVAL_MS,
    refetchIntervalInBackground: options?.refetchIntervalInBackground ?? false,
  });
}

export function useUsageTrends(
  range: UsageRangeSelection,
  filters?: UsageScopeFilters,
  options?: UsageQueryOptions,
) {
  const effective = normalizeScopeFilters(filters);
  const hostId = useResolvedHostId(options);
  return useQuery({
    queryKey: usageKeys.trends(
      hostId,
      range.preset,
      range.customStartDate,
      range.customEndDate,
      effective,
      range.liveEndTime,
    ),
    queryFn: () => {
      const { startDate, endDate } = resolveUsageRange(range);
      return usageApi.getUsageTrends(
        startDate,
        endDate,
        effective.appType,
        effective.providerName,
        effective.model,
        hostArg(hostId),
      );
    },
    enabled: options?.enabled ?? true,
    placeholderData: keepPreviousData,
    refetchInterval: options?.refetchInterval ?? DEFAULT_REFETCH_INTERVAL_MS,
    refetchIntervalInBackground: options?.refetchIntervalInBackground ?? false,
  });
}

/** 最早有用量记录的日期，「全部」的按年热力图据此决定从哪一年画起 */
export function useUsageFirstDate(
  filters?: UsageScopeFilters,
  options?: UsageQueryOptions,
) {
  const effective = normalizeScopeFilters(filters);
  const hostId = useResolvedHostId(options);
  return useQuery({
    queryKey: usageKeys.firstDate(hostId, effective),
    queryFn: () =>
      usageApi.getUsageFirstDate(
        effective.appType,
        effective.providerName,
        effective.model,
        hostArg(hostId),
      ),
    placeholderData: keepPreviousData,
    refetchInterval: options?.refetchInterval ?? DEFAULT_REFETCH_INTERVAL_MS,
    refetchIntervalInBackground: options?.refetchIntervalInBackground ?? false,
  });
}

export function useProviderStats(
  range: UsageRangeSelection,
  filters?: UsageScopeFilters,
  options?: UsageQueryOptions,
) {
  const effective = normalizeScopeFilters(filters);
  const hostId = useResolvedHostId(options);
  return useQuery({
    queryKey: usageKeys.providerStats(
      hostId,
      range.preset,
      range.customStartDate,
      range.customEndDate,
      effective,
      range.liveEndTime,
    ),
    queryFn: () => {
      const { startDate, endDate } = resolveUsageRange(range);
      return usageApi.getProviderStats(
        startDate,
        endDate,
        effective.appType,
        effective.providerName,
        effective.model,
        hostArg(hostId),
      );
    },
    placeholderData: keepPreviousData,
    enabled: options?.enabled ?? true,
    refetchInterval: options?.refetchInterval ?? DEFAULT_REFETCH_INTERVAL_MS,
    refetchIntervalInBackground: options?.refetchIntervalInBackground ?? false,
  });
}

export function useModelStats(
  range: UsageRangeSelection,
  filters?: UsageScopeFilters,
  options?: UsageQueryOptions,
) {
  const effective = normalizeScopeFilters(filters);
  const hostId = useResolvedHostId(options);
  return useQuery({
    queryKey: usageKeys.modelStats(
      hostId,
      range.preset,
      range.customStartDate,
      range.customEndDate,
      effective,
      range.liveEndTime,
    ),
    queryFn: () => {
      const { startDate, endDate } = resolveUsageRange(range);
      return usageApi.getModelStats(
        startDate,
        endDate,
        effective.appType,
        effective.providerName,
        effective.model,
        hostArg(hostId),
      );
    },
    placeholderData: keepPreviousData,
    enabled: options?.enabled ?? true,
    refetchInterval: options?.refetchInterval ?? DEFAULT_REFETCH_INTERVAL_MS,
    refetchIntervalInBackground: options?.refetchIntervalInBackground ?? false,
  });
}

export function useRequestLogs({
  filters,
  range,
  page = 0,
  pageSize = 20,
  options,
}: RequestLogsQueryArgs) {
  const hostId = useResolvedHostId(options);
  const key: RequestLogsKey = {
    preset: range.preset,
    customStartDate: range.customStartDate,
    customEndDate: range.customEndDate,
    liveEndTime: range.liveEndTime,
    appType: filters.appType,
    providerName: filters.providerName,
    model: filters.model,
    statusCode: filters.statusCode,
    projectDir: filters.projectDir,
  };

  return useQuery({
    queryKey: usageKeys.logs(hostId, key, page, pageSize),
    queryFn: () => {
      const effectiveFilters = { ...filters, ...resolveUsageRange(range) };
      return usageApi.getRequestLogs(
        effectiveFilters,
        page,
        pageSize,
        hostArg(hostId),
      );
    },
    placeholderData: keepPreviousData,
    enabled: options?.enabled ?? true,
    refetchInterval: options?.refetchInterval ?? DEFAULT_REFETCH_INTERVAL_MS, // 每30秒自动刷新
    refetchIntervalInBackground: options?.refetchIntervalInBackground ?? false,
  });
}

export function useRequestDetail(requestId: string) {
  return useQuery({
    queryKey: usageKeys.detail(requestId),
    queryFn: () => usageApi.getRequestDetail(requestId),
    enabled: !!requestId,
  });
}

/**
 * 会话日志扫描（后台定时或手动同步）最近一次完成的时间（毫秒）。
 * 默认 30 秒查询一次；用量页由统一刷新流程在扫描结束后失效重取。
 */
export function useSessionUsageLastSync(options?: UsageQueryOptions) {
  return useQuery({
    queryKey: [...usageKeys.all, "session-last-sync"] as const,
    queryFn: () => usageApi.getSessionUsageLastSync(),
    refetchInterval: options?.refetchInterval ?? DEFAULT_REFETCH_INTERVAL_MS,
    refetchIntervalInBackground: options?.refetchIntervalInBackground ?? false,
  });
}

/** 单个会话的用量汇总（会话阅读页头部），只数会话日志导入的行。 */
export function useSessionUsageSummary(appType: string, sessionId: string) {
  return useQuery({
    queryKey: usageKeys.session(appType, sessionId),
    queryFn: () => usageApi.getSessionUsageSummary(appType, sessionId),
    refetchInterval: DEFAULT_REFETCH_INTERVAL_MS,
    refetchIntervalInBackground: false,
  });
}

export function useModelPricing() {
  return useQuery({
    queryKey: usageKeys.pricing(),
    queryFn: usageApi.getModelPricing,
  });
}

export function useProviderLimits(providerId: string, appType: string) {
  return useQuery({
    queryKey: usageKeys.limits(providerId, appType),
    queryFn: () => usageApi.checkProviderLimits(providerId, appType),
    enabled: !!providerId && !!appType,
  });
}

export function useUpdateModelPricing() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (params: {
      modelId: string;
      displayName: string;
      inputCost: string;
      outputCost: string;
      cacheReadCost: string;
      cacheCreationCost: string;
    }) =>
      usageApi.updateModelPricing(
        params.modelId,
        params.displayName,
        params.inputCost,
        params.outputCost,
        params.cacheReadCost,
        params.cacheCreationCost,
      ),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: usageKeys.all });
    },
  });
}

export function useDeleteModelPricing() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (modelId: string) => usageApi.deleteModelPricing(modelId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: usageKeys.all });
    },
  });
}
