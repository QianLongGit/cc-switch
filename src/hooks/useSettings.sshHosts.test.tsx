import { act, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { beforeEach, describe, expect, it } from "vitest";
import { getSettings, setSettings } from "../../tests/msw/state";
import type { SshHostConfig } from "@/types";
import { useSettings } from "./useSettings";

// ============================================================================
// sshHosts 透传防清空（Review Focus #5 前端半场）
//
// 后端 save_settings 是全量替换：payload 里缺 sshHosts 就等于把已配置的
// 远程主机清空。这里走 msw 模拟的完整保存链路（invoke → save_settings），
// 验证改任何无关字段时 sshHosts 原样往返。
// ============================================================================

const HOST: SshHostConfig = {
  id: "h1",
  label: "build-box",
  host: "192.168.1.10",
  user: "dev",
  port: 2222,
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
    const { result } = renderSettingsHook();
    await waitFor(() => expect(result.current.settings).not.toBeNull());

    await act(async () => {
      await result.current.autoSaveSettings({ language: "en" });
    });

    expect(getSettings().sshHosts).toEqual([HOST]);
  });

  it("saveSettings 手动保存（数据页「保存」同款路径）同样不丢 sshHosts", async () => {
    const { result } = renderSettingsHook();
    await waitFor(() => expect(result.current.settings).not.toBeNull());

    await act(async () => {
      await result.current.saveSettings(undefined, { silent: true });
    });

    expect(getSettings().sshHosts).toEqual([HOST]);
  });
});
