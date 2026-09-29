import { beforeEach, describe, expect, it, vi } from "vitest";

// ============================================================================
// IPC 契约测试：锁定命令名与参数形状（前后端契约的唯一自动防线）
//
// Tauri 2 对 Rust 命令参数做 snake_case → camelCase 自动映射：
//   project_path: String  ←→  invoke("...", { projectPath: ... })
// 参数名写错即为静默 undefined，只能在调用点锁定，别处测不出来。
// ============================================================================

const invokeMock = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
}));

import { projectRoutingApi } from "./projectRouting";

describe("projectRoutingApi IPC 契约", () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it("listProjects 调 list_projects 命令且无参", async () => {
    const rows = [
      {
        projectPath: "/Users/dev/p1",
        basename: "p1",
        lastActiveAt: 1759200000,
        hasActiveSession: true,
        boundProviderId: "pid-1",
        boundProviderName: "Provider A",
        boundProviderValid: true,
        syncStatus: "synced",
      },
    ];
    invokeMock.mockResolvedValue(rows);

    const result = await projectRoutingApi.listProjects();

    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("list_projects");
    expect(result).toEqual(rows);
  });

  it("setProjectRoute 以 camelCase 参数调 set_project_route", async () => {
    invokeMock.mockResolvedValue(undefined);

    await projectRoutingApi.setProjectRoute("/Users/dev/p1", "pid-1");

    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("set_project_route", {
      projectPath: "/Users/dev/p1",
      providerId: "pid-1",
    });
  });

  it("clearProjectRoute 以 camelCase 参数调 clear_project_route", async () => {
    invokeMock.mockResolvedValue(undefined);

    await projectRoutingApi.clearProjectRoute("/Users/dev/p1");

    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("clear_project_route", {
      projectPath: "/Users/dev/p1",
    });
  });
});
