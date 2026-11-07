import { act, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import type { ReactNode } from "react";
import { beforeEach, describe, expect, it } from "vitest";
import { server } from "../../tests/msw/server";
import { setSettings } from "../../tests/msw/state";
import type { Settings, SshHostConfig } from "@/types";
import { useSettings } from "./useSettings";

// ============================================================================
// sshHosts 透传防清空（Review Focus #5 前端半场）
//
// 后端 save_settings 是全量替换：payload 里缺 sshHosts 就等于把已配置的
// 远程主机清空。断言必须落在**发出去的 payload** 上——msw state 的
// setSettings 是合并语义，key 缺失的回归形态会被旧值掩盖，闸会失灵。
// 因此测试内 server.use 临时覆写 save_settings，捕获请求体逐字段核对。
// ============================================================================

const HOST: SshHostConfig = {
  id: "h1",
  label: "build-box",
  host: "192.168.1.10",
  user: "dev",
  port: 2222,
};

/** 覆写 save_settings：捕获发出的 settings payload（resetHandlers 会还原） */
const captureSavedPayloads = (): Settings[] => {
  const captured: Settings[] = [];
  server.use(
    http.post("http://tauri.local/save_settings", async ({ request }) => {
      const body = (await request.json()) as { settings: Settings };
      captured.push(body.settings);
      return HttpResponse.json(true);
    }),
  );
  return captured;
};

const renderSettingsHook = () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return renderHook(() => useSettings(), { wrapper });
};

describe("useSettings sshHosts 透传", () => {
  beforeEach(() => {
    setSettings({ sshHosts: [HOST] });
  });

  it("autoSaveSettings 保存无关字段（语言）时 payload 原样携带 sshHosts", async () => {
    const payloads = captureSavedPayloads();
    const { result } = renderSettingsHook();
    await waitFor(() => expect(result.current.settings).not.toBeNull());

    await act(async () => {
      await result.current.autoSaveSettings({ language: "en" });
    });

    expect(payloads).toHaveLength(1);
    // 直接对请求体断言：回归形态若是 payload 里 key 被剔除，这里拿到 undefined
    expect(payloads[0].sshHosts).toEqual([HOST]);
  });

  it("saveSettings 手动保存（数据页「保存」同款路径）payload 同样不丢 sshHosts", async () => {
    const payloads = captureSavedPayloads();
    const { result } = renderSettingsHook();
    await waitFor(() => expect(result.current.settings).not.toBeNull());

    await act(async () => {
      await result.current.saveSettings(undefined, { silent: true });
    });

    expect(payloads).toHaveLength(1);
    expect(payloads[0].sshHosts).toEqual([HOST]);
  });
});
