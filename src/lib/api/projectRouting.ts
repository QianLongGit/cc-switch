import { invoke } from "@tauri-apps/api/core";
import type { ProjectRouteInfo } from "@/types/projectRouting";

export const projectRoutingApi = {
  // ========== 项目路由 API ==========

  // 获取项目路由列表（扫描 + 绑定 + 同步状态合并）
  async listProjects(): Promise<ProjectRouteInfo[]> {
    return invoke("list_projects");
  },

  // 绑定项目 → 供应商（DB + settings.local.json 双写）
  async setProjectRoute(
    projectPath: string,
    providerId: string,
  ): Promise<void> {
    return invoke("set_project_route", { projectPath, providerId });
  },

  // 解除绑定（DB 行 + 文件目标行双清）
  async clearProjectRoute(projectPath: string): Promise<void> {
    return invoke("clear_project_route", { projectPath });
  },
};
