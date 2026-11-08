import type { ReactNode } from "react";
import { act, renderHook, waitFor } from "@testing-library/react";
import { QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  sessionContentKeys,
  useSessionContentSearch,
} from "@/hooks/useSessionContentSearch";
import { useSessionsQuery } from "@/lib/query/queries";
import { sessionsApi } from "@/lib/api/sessions";
import type { SessionMeta } from "@/types";
import { createTestQueryClient } from "../utils/testQueryClient";

vi.mock("@/lib/api/sessions", () => ({
  sessionsApi: {
    list: vi.fn(),
    syncContentIndex: vi.fn(),
    searchContent: vi.fn(),
  },
}));

/** 用例级覆盖项：远程主机 id 与搜索词（缺省保持本机 + 空词，既有用例不受影响） */
interface SearchOverrides {
  hostId?: string;
  query?: string;
}

function renderSearch(enabled = true, overrides: SearchOverrides = {}) {
  const client = createTestQueryClient();
  return renderHook(
    ({ enabled }) => {
      const list = useSessionsQuery();
      const options = {
        query: overrides.query ?? "",
        enabled,
        listIsSuccess: list.isSuccess,
        listUpdatedAt: list.dataUpdatedAt,
        providerIds: ["claude"],
        hostId: overrides.hostId,
      };
      useSessionContentSearch(options);
      return { ...list };
    },
    {
      initialProps: { enabled },
      wrapper: ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={client}>{children}</QueryClientProvider>
      ),
    },
  );
}

const session: SessionMeta = {
  providerId: "claude",
  sessionId: "last-session",
  sourcePath: "/test/last-session.jsonl",
};

describe("useSessionContentSearch index synchronization", () => {
  beforeEach(() => {
    vi.mocked(sessionsApi.list).mockReset();
    vi.mocked(sessionsApi.syncContentIndex).mockReset().mockResolvedValue({
      running: false,
      processed: 0,
      total: 0,
    });
  });

  it("waits for a successful list read before synchronizing an empty list", async () => {
    let resolveList!: (sessions: SessionMeta[]) => void;
    vi.mocked(sessionsApi.list).mockReturnValue(
      new Promise((resolve) => {
        resolveList = resolve;
      }),
    );
    const { result } = renderSearch();
    expect(result.current.isPending).toBe(true);
    expect(result.current.dataUpdatedAt).toBe(0);
    expect(sessionsApi.syncContentIndex).not.toHaveBeenCalled();

    await act(async () => resolveList([]));
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    await waitFor(() =>
      expect(sessionsApi.syncContentIndex).toHaveBeenCalledTimes(1),
    );
  });

  it("synchronizes after an external deletion removes the last session", async () => {
    vi.mocked(sessionsApi.list)
      .mockResolvedValueOnce([session])
      .mockResolvedValueOnce([]);
    const { result } = renderSearch();
    await waitFor(() =>
      expect(sessionsApi.syncContentIndex).toHaveBeenCalledTimes(1),
    );

    await act(async () => {
      await result.current.refetch();
    });
    await waitFor(() => expect(result.current.data).toEqual([]));
    await waitFor(() =>
      expect(sessionsApi.syncContentIndex).toHaveBeenCalledTimes(2),
    );
  });

  it("does not synchronize when the initial list read fails", async () => {
    vi.mocked(sessionsApi.list).mockRejectedValue(new Error("scan failed"));
    const { result } = renderSearch();
    await waitFor(() => expect(result.current.isError).toBe(true));
    expect(result.current.dataUpdatedAt).toBe(0);
    expect(sessionsApi.syncContentIndex).not.toHaveBeenCalled();
  });

  it("does not synchronize again when a list refresh fails", async () => {
    vi.mocked(sessionsApi.list)
      .mockResolvedValueOnce([session])
      .mockRejectedValueOnce(new Error("scan failed"));
    const { result } = renderSearch();
    await waitFor(() =>
      expect(sessionsApi.syncContentIndex).toHaveBeenCalledTimes(1),
    );
    const updatedAt = result.current.dataUpdatedAt;
    await act(async () => {
      await result.current.refetch();
    });
    await waitFor(() => expect(result.current.isError).toBe(true));
    expect(result.current.dataUpdatedAt).toBe(updatedAt);
    expect(sessionsApi.syncContentIndex).toHaveBeenCalledTimes(1);
  });

  it("waits for recovery when enabled after a failed refresh", async () => {
    vi.mocked(sessionsApi.list)
      .mockResolvedValueOnce([])
      .mockRejectedValueOnce(new Error("scan failed"))
      .mockResolvedValueOnce([]);
    const { result, rerender } = renderSearch(false);
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    const updatedAt = result.current.dataUpdatedAt;
    await act(async () => {
      await result.current.refetch();
    });
    await waitFor(() => expect(result.current.isError).toBe(true));
    expect(result.current.dataUpdatedAt).toBe(updatedAt);
    expect(updatedAt).toBeGreaterThan(0);
    rerender({ enabled: true });
    expect(sessionsApi.syncContentIndex).not.toHaveBeenCalled();

    await act(async () => {
      await result.current.refetch();
    });
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    await waitFor(() =>
      expect(sessionsApi.syncContentIndex).toHaveBeenCalledTimes(1),
    );
  });

  it("only synchronizes a loaded empty list once search is enabled", async () => {
    vi.mocked(sessionsApi.list).mockResolvedValue([]);
    const { result, rerender } = renderSearch(false);
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(sessionsApi.syncContentIndex).not.toHaveBeenCalled();
    rerender({ enabled: true });
    await waitFor(() =>
      expect(sessionsApi.syncContentIndex).toHaveBeenCalledTimes(1),
    );
  });
});

// ============================================================================
// 远程主机接线（迭代 2 · Task 5，设计 §4.2「前端显式改动清单」三处）
//
//   1. query key 扩 hostId 维度 —— 切主机不串缓存，缺省占位 "local"
//      对齐 sessionKeys.list 扩维先例
//   2. searchContent 透传 hostId —— 后端按 hostId 转远端 rg/grep 路线
//   3. 远程下跳过 listUpdatedAt 触发的本机索引同步 —— sync_session_index
//      只维护本机 FTS5 索引，远程命中由后端现算，同步纯属白跑
// ============================================================================

describe("useSessionContentSearch remote host wiring", () => {
  beforeEach(() => {
    vi.mocked(sessionsApi.list).mockReset().mockResolvedValue([session]);
    vi.mocked(sessionsApi.syncContentIndex).mockReset().mockResolvedValue({
      running: false,
      processed: 0,
      total: 0,
    });
    vi.mocked(sessionsApi.searchContent).mockReset().mockResolvedValue({
      hits: [],
      status: { running: false, processed: 0, total: 0 },
    });
  });

  it("distinguishes cache keys per host; omitted hostId occupies the local slot", () => {
    const providers = ["claude"];
    // 主机不同 → 键不同：切主机不得复用旧主机的搜索缓存
    expect(sessionContentKeys.search("q", providers, "h1")).not.toEqual(
      sessionContentKeys.search("q", providers, "h2"),
    );
    // 缺省（本机）与显式 "local" 同键，对齐 sessionKeys.list 的占位先例
    expect(sessionContentKeys.search("q", providers)).toEqual(
      sessionContentKeys.search("q", providers, "local"),
    );
  });

  it("passes hostId through to searchContent when set", async () => {
    renderSearch(true, { hostId: "h1", query: "needle" });

    await waitFor(() =>
      expect(sessionsApi.searchContent).toHaveBeenCalledWith(
        "needle",
        ["claude"],
        "h1",
      ),
    );
  });

  it("skips local index sync while searching a remote host", async () => {
    const { result } = renderSearch(true, { hostId: "h1" });
    await waitFor(() => expect(result.current.isSuccess).toBe(true));

    // 列表刷新（dataUpdatedAt 前进）后同样不得触发本机索引同步
    const firstUpdatedAt = result.current.dataUpdatedAt;
    await act(async () => {
      await result.current.refetch();
    });
    await waitFor(() =>
      expect(result.current.dataUpdatedAt).toBeGreaterThan(firstUpdatedAt),
    );
    // 再 flush 一轮，防止误触的 effect 尚未执行造成假阴性
    await act(async () => {});
    expect(sessionsApi.syncContentIndex).not.toHaveBeenCalled();
  });
});
