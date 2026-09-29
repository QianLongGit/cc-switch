// ============================================================================
// 【一次性冒烟测试】ProjectRoutingPage 渲染冒烟——验证 T11 页面组件
// 挂载 / 行渲染 / 徽标 / 真值表按钮接线。跑绿后即删除，不入库。
// （测试 setup 的 i18n 资源为空，t() 返回 key 原文，断言按 key 匹配）
// ============================================================================

import { render, screen } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { describe, expect, it } from "vitest";
import { QueryClientProvider } from "@tanstack/react-query";
import { ProjectRoutingPage } from "@/components/projectRouting/ProjectRoutingPage";
import { server } from "../msw/server";
import { createTestQueryClient } from "../utils/testQueryClient";

const now = Math.floor(Date.now() / 1000);

const projects = [
  {
    projectPath: "/Users/dev/Project/alpha",
    basename: "alpha",
    lastActiveAt: now - 60,
    hasActiveSession: true,
    boundProviderId: "p-1",
    boundProviderName: "Provider One",
    boundProviderValid: true,
    syncStatus: "synced",
  },
  {
    projectPath: "/Users/dev/Project/beta",
    basename: "beta",
    lastActiveAt: now - 3600,
    hasActiveSession: false,
    boundProviderId: undefined,
    boundProviderName: undefined,
    boundProviderValid: true,
    syncStatus: "orphan_header",
  },
];

describe("ProjectRoutingPage 冒烟", () => {
  it("渲染项目行 / 使用中徽标 / 同步徽标与真值表按钮", async () => {
    server.use(
      http.post("http://tauri.local/list_projects", () =>
        HttpResponse.json(projects),
      ),
      http.post("http://tauri.local/get_providers", () =>
        HttpResponse.json({
          "p-1": {
            id: "p-1",
            name: "Provider One",
            category: "custom",
            settingsConfig: { env: {} },
          },
        }),
      ),
      http.post("http://tauri.local/get_current_provider", () =>
        HttpResponse.json("p-1"),
      ),
    );

    render(
      <QueryClientProvider client={createTestQueryClient()}>
        <ProjectRoutingPage />
      </QueryClientProvider>,
    );

    // 两行项目按 basename 渲染
    expect(await screen.findByText("alpha")).toBeInTheDocument();
    expect(screen.getByText("beta")).toBeInTheDocument();

    // 使用中徽标（活跃会话行）
    expect(screen.getByText("projectRouting.inUse")).toBeInTheDocument();

    // synced 行：无操作按钮；orphan 行：显示提示 + 清除按钮，无修复按钮
    expect(screen.getByText("projectRouting.sync.orphan")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "projectRouting.sync.clear" }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "projectRouting.sync.fix" }),
    ).not.toBeInTheDocument();

    // 每行一个供应商 Select（combobox）
    expect(screen.getAllByRole("combobox")).toHaveLength(2);
  });

  it("空态渲染引导文案", async () => {
    server.use(
      http.post("http://tauri.local/list_projects", () =>
        HttpResponse.json([]),
      ),
      http.post("http://tauri.local/get_providers", () =>
        HttpResponse.json({}),
      ),
      http.post("http://tauri.local/get_current_provider", () =>
        HttpResponse.json(""),
      ),
    );

    render(
      <QueryClientProvider client={createTestQueryClient()}>
        <ProjectRoutingPage />
      </QueryClientProvider>,
    );

    expect(
      await screen.findByText("projectRouting.empty"),
    ).toBeInTheDocument();
    expect(screen.queryByRole("combobox")).not.toBeInTheDocument();
  });

  it("invalid 绑定行展示已失效占位（不静默清空）", async () => {
    server.use(
      http.post("http://tauri.local/list_projects", () =>
        HttpResponse.json([
          {
            projectPath: "/Users/dev/Project/gone",
            basename: "gone",
            lastActiveAt: now - 120,
            hasActiveSession: false,
            boundProviderId: "p-deleted",
            boundProviderName: "Deleted Provider",
            boundProviderValid: false,
            syncStatus: "out_of_sync",
          },
        ]),
      ),
      http.post("http://tauri.local/get_providers", () =>
        HttpResponse.json({}),
      ),
      http.post("http://tauri.local/get_current_provider", () =>
        HttpResponse.json(""),
      ),
    );

    render(
      <QueryClientProvider client={createTestQueryClient()}>
        <ProjectRoutingPage />
      </QueryClientProvider>,
    );

    // Select 无匹配项 → placeholder 显示 "名称 · 已失效"
    expect(
      await screen.findByText("Deleted Provider · projectRouting.invalid"),
    ).toBeInTheDocument();
    // out_of_sync（有绑定）→ 修复按钮出现
    expect(
      screen.getByRole("button", { name: "projectRouting.sync.fix" }),
    ).toBeInTheDocument();
    expect(screen.getByText("projectRouting.sync.outOfSync")).toBeInTheDocument();
  });
});
