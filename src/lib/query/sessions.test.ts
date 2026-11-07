import { createElement, type ReactNode } from "react";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { describe, it, expect, vi } from "vitest";
import { sessionsApi } from "@/lib/api";
import { sessionKeys } from "./sessions";
import { useSessionsQuery } from "./queries";

vi.mock("@/lib/api", () => ({
  sessionsApi: { list: vi.fn(async () => []) },
}));

describe("sessionKeys host 扩维", () => {
  it("list() 默认本机 key", () => {
    expect(sessionKeys.list()).toEqual(["sessions", "local"]);
  });

  it('list("h1") 按主机分维', () => {
    expect(sessionKeys.list("h1")).toEqual(["sessions", "h1"]);
  });

  it("all 作为前缀形态覆盖所有主机变体", () => {
    expect(sessionKeys.all).toEqual(["sessions"]);
  });

  it("既有消息 / 转录 key 不回归", () => {
    expect(sessionKeys.messages("claude", "/a.jsonl")).toEqual([
      "sessionMessages",
      "claude",
      "/a.jsonl",
    ]);
    expect(sessionKeys.transcript("claude", "/a.jsonl")).toEqual([
      "sessionTranscript",
      "claude",
      "/a.jsonl",
    ]);
  });
});

// ─── useSessionsQuery 的 hostId 透传（主机切换的数据层契约） ────────────────

const createWrapper = () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return ({ children }: { children: ReactNode }) =>
    createElement(QueryClientProvider, { client }, children);
};

describe("useSessionsQuery hostId 透传", () => {
  it("传 hostId 时按该主机列会话", async () => {
    const { result } = renderHook(() => useSessionsQuery("h1"), {
      wrapper: createWrapper(),
    });
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(sessionsApi.list).toHaveBeenCalledWith("h1");
  });

  it("不传 hostId 时按本机列出", async () => {
    const { result } = renderHook(() => useSessionsQuery(), {
      wrapper: createWrapper(),
    });
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(sessionsApi.list).toHaveBeenCalledWith(undefined);
  });
});
