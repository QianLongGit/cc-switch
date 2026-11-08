import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import i18n from "i18next";
import { http, HttpResponse } from "msw";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import zh from "@/i18n/locales/zh.json";
import { server } from "../../../tests/msw/server";
import { setSettings } from "../../../tests/msw/state";
import { emitTauriEvent } from "../../../tests/msw/tauriMocks";
import type { SshHostConfig } from "@/types";
import type { RemoteUsageSyncStatus, UsageSummary } from "@/types/usage";
import { usageKeys } from "@/lib/query/usage";
import { UsageDashboard } from "./UsageDashboard";

// ============================================================================
// Task 9：usage 页主机切换与远程同步 UI
//
// 集成面：filterRow 挂 SessionHostMenu → hostId 维度查询（本机缺省零回归）
// → peek 首扫预告 → sync(force=true) 确认 → remote-usage-sync-status 进度
// 渲染 → 取消 → 远程错误态 + 重试。命令经全局 tauriMocks 走 msw，
// 断言全部落在捕获的请求体与渲染文本上。
// ============================================================================

const TAURI = "http://tauri.local";

const HOSTS: SshHostConfig[] = [
  { id: "h1", label: "build-box", host: "192.168.1.10", user: "dev" },
];

const EMPTY_SUMMARY: UsageSummary = {
  totalRequests: 0,
  totalCost: "0",
  totalInputTokens: 0,
  totalOutputTokens: 0,
  totalCacheCreationTokens: 0,
  totalCacheReadTokens: 0,
  successRate: 0,
  realTotalTokens: 0,
  cacheHitRate: 0,
};

const LOCAL_SUMMARY: UsageSummary = { ...EMPTY_SUMMARY, totalRequests: 5 };

const PEEK_FIRST_SYNC: RemoteUsageSyncStatus = {
  doneFiles: 0,
  totalFiles: 12,
  doneBytes: 52_428_800,
  skipped: 0,
};

/** 每个用例独立的命令捕获与可变状态（msw resetHandlers 在 afterEach 还原覆写） */
const usageCommands = () => {
  const state = {
    remoteSummary: EMPTY_SUMMARY,
    failRemote: false,
    summaryBodies: [] as Record<string, unknown>[],
    peekBodies: [] as Record<string, unknown>[],
    syncBodies: [] as Record<string, unknown>[],
    cancelBodies: [] as Record<string, unknown>[],
    resolveSync: undefined as
      | ((status: RemoteUsageSyncStatus) => void)
      | undefined,
  };

  server.use(
    http.post(`${TAURI}/check_ssh_availability`, () => HttpResponse.json(true)),
    // 空数据兜底：整树渲染时各表格 / 图表的查询都要有应答
    http.post(`${TAURI}/get_usage_summary_by_app`, () => HttpResponse.json([])),
    http.post(`${TAURI}/get_usage_trends`, () => HttpResponse.json([])),
    http.post(`${TAURI}/get_usage_first_date`, () => HttpResponse.json(null)),
    http.post(`${TAURI}/get_provider_stats`, () => HttpResponse.json([])),
    http.post(`${TAURI}/get_model_stats`, () => HttpResponse.json([])),
    http.post(`${TAURI}/get_request_logs`, () =>
      HttpResponse.json({ data: [], total: 0, page: 0, pageSize: 20 }),
    ),
    http.post(`${TAURI}/get_usage_summary`, async ({ request }) => {
      const body = (await request.json()) as Record<string, unknown>;
      state.summaryBodies.push(body);
      if (body.hostId === "h1") {
        if (state.failRemote) {
          return new HttpResponse("SSH 主机「build-box」: 连接超时", {
            status: 500,
          });
        }
        return HttpResponse.json(state.remoteSummary);
      }
      return HttpResponse.json(LOCAL_SUMMARY);
    }),
    http.post(`${TAURI}/peek_remote_usage`, async ({ request }) => {
      state.peekBodies.push((await request.json()) as Record<string, unknown>);
      return HttpResponse.json(PEEK_FIRST_SYNC);
    }),
    http.post(`${TAURI}/sync_remote_usage`, async ({ request }) => {
      state.syncBodies.push((await request.json()) as Record<string, unknown>);
      // 挂起直到用例显式放行，同步期间才能验证进度与取消
      return HttpResponse.json(
        await new Promise<RemoteUsageSyncStatus>((resolve) => {
          state.resolveSync = resolve;
        }),
      );
    }),
    http.post(`${TAURI}/cancel_remote_usage_sync`, async ({ request }) => {
      state.cancelBodies.push(
        (await request.json()) as Record<string, unknown>,
      );
      return HttpResponse.json(null);
    }),
  );

  return state;
};

const renderDashboard = () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <UsageDashboard sessionAutoSyncEnabled={false} settingsReady />
    </QueryClientProvider>,
  );
};

/** 打开主机菜单选中 build-box（h1）；settings 异步到达后菜单才渲染 */
const selectHost = async (user: ReturnType<typeof userEvent.setup>) => {
  const trigger = await screen.findByRole("button", { name: /本机/ });
  await user.click(trigger);
  await user.click(screen.getByRole("menuitemradio", { name: /build-box/ }));
};

beforeAll(() => {
  // setupTests 的 i18n 是空壳资源；挂上 zh 文案后断言可以核对真实 key
  i18n.addResourceBundle("zh", "translation", zh, true, true);
});

beforeEach(() => {
  setSettings({ sshHosts: HOSTS });
});

describe("usageKeys host 维度", () => {
  it("远程主机与本机的 key 不同维，缺省 hostId 用 local 占位", () => {
    const remote = usageKeys.summary(
      "h1",
      "today",
      undefined,
      undefined,
      undefined,
      false,
    );
    const local = usageKeys.summary(
      null,
      "today",
      undefined,
      undefined,
      undefined,
      false,
    );
    expect(remote).not.toEqual(local);
    expect(remote[1]).toBe("h1");
    expect(local[1]).toBe("local");
    // 主机维必须进 key：切换主机时缓存不串
    for (const factory of [
      (hostId: string | null) =>
        usageKeys.summaryByApp(
          hostId,
          "today",
          undefined,
          undefined,
          undefined,
          false,
        ),
      (hostId: string | null) =>
        usageKeys.trends(hostId, "today", undefined, undefined, undefined, false),
      (hostId: string | null) => usageKeys.firstDate(hostId, undefined),
      (hostId: string | null) =>
        usageKeys.providerStats(
          hostId,
          "today",
          undefined,
          undefined,
          undefined,
          false,
        ),
      (hostId: string | null) =>
        usageKeys.modelStats(
          hostId,
          "today",
          undefined,
          undefined,
          undefined,
          false,
        ),
      (hostId: string | null) =>
        usageKeys.logs(
          hostId,
          {
            preset: "today",
            liveEndTime: false,
          },
          0,
          20,
        ),
    ]) {
      expect(factory("h1")).not.toEqual(factory(null));
      expect(factory(null)[1]).toBe("local");
    }
  });
});

describe("UsageDashboard 主机切换与远程同步", () => {
  it("hostId 缺省（本机）时数据请求不带 hostId 字段", async () => {
    const state = usageCommands();
    renderDashboard();
    await waitFor(() => expect(state.summaryBodies.length).toBeGreaterThan(0));
    for (const body of state.summaryBodies) {
      expect(body).not.toHaveProperty("hostId");
    }
  });

  it("选中远程主机后数据请求带 hostId，并出现首扫预告与开始按钮", async () => {
    const state = usageCommands();
    const user = userEvent.setup();
    renderDashboard();
    await selectHost(user);

    await waitFor(() =>
      expect(state.summaryBodies.some((body) => body.hostId === "h1")).toBe(
        true,
      ),
    );
    await waitFor(() => expect(state.peekBodies).toEqual([{ hostId: "h1" }]));

    // 首扫预告：12 个文件 / 约 50.0 MB（52,428,800 bytes）
    expect(
      await screen.findByText(/首次同步需拉取 12 个文件 \/ 约 50\.0 MB/),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "开始同步" }),
    ).toBeInTheDocument();
  });

  it("确认首扫后调用 sync_remote_usage(force=true)，进度事件渲染，完成后预告消失", async () => {
    const state = usageCommands();
    const user = userEvent.setup();
    renderDashboard();
    await selectHost(user);

    await user.click(await screen.findByRole("button", { name: "开始同步" }));
    await waitFor(() =>
      expect(state.syncBodies).toEqual([{ hostId: "h1", force: true }]),
    );

    // 同步挂起期间收到进度事件 → 渲染 doneFiles/totalFiles 与字节
    act(() => {
      emitTauriEvent("remote-usage-sync-status", {
        doneFiles: 3,
        totalFiles: 12,
        doneBytes: 1_048_576,
        skipped: 0,
      });
    });
    expect(await screen.findByText(/3\/12/)).toBeInTheDocument();
    expect(screen.getByText(/1\.0 MB/)).toBeInTheDocument();

    // 完成放行：远程已有数据 → 预告与进度收起
    state.remoteSummary = LOCAL_SUMMARY;
    await act(async () => {
      state.resolveSync?.({ ...PEEK_FIRST_SYNC, doneFiles: 12, skipped: 1 });
    });
    await waitFor(() =>
      expect(screen.queryByText(/首次同步需拉取/)).toBeNull(),
    );
    expect(screen.queryByText(/3\/12/)).toBeNull();
  });

  it("同步进行中可取消：点击取消调用 cancel_remote_usage_sync", async () => {
    const state = usageCommands();
    const user = userEvent.setup();
    renderDashboard();
    await selectHost(user);

    await user.click(await screen.findByRole("button", { name: "开始同步" }));
    expect(
      await screen.findByRole("button", { name: "取消同步" }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "取消同步" }));
    await waitFor(() => expect(state.cancelBodies).toHaveLength(1));
  });

  it("远程加载失败显示分类错误与重试；重试重新发起请求", async () => {
    const state = usageCommands();
    state.failRemote = true;
    const user = userEvent.setup();
    renderDashboard();
    await selectHost(user);

    // 错误条带后端的 SSH 错误文案
    expect(await screen.findByText(/远程用量加载失败/)).toBeInTheDocument();
    expect(screen.getByText(/连接超时/)).toBeInTheDocument();

    const callsBefore = state.summaryBodies.filter(
      (body) => body.hostId === "h1",
    ).length;
    await user.click(screen.getByRole("button", { name: "重试" }));
    await waitFor(() =>
      expect(
        state.summaryBodies.filter((body) => body.hostId === "h1").length,
      ).toBeGreaterThan(callsBefore),
    );
  });

  it("增量态（远程已有数据）不显示首扫预告，手动同步走 force=false", async () => {
    const state = usageCommands();
    state.remoteSummary = LOCAL_SUMMARY;
    const user = userEvent.setup();
    renderDashboard();
    await selectHost(user);

    await waitFor(() =>
      expect(state.summaryBodies.some((body) => body.hostId === "h1")).toBe(
        true,
      ),
    );
    // 远程已有数据：无首扫预告
    await waitFor(() =>
      expect(screen.queryByText(/首次同步需拉取/)).toBeNull(),
    );

    // 页头「立即同步」远程下走轻量增量（force=false）
    await user.click(
      screen.getByRole("button", { name: /立即同步|刚刚同步|同步中/ }),
    );
    await waitFor(() =>
      expect(state.syncBodies).toEqual([{ hostId: "h1", force: false }]),
    );
  });
});
