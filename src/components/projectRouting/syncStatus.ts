// ============================================================================
// 项目路由页面的纯逻辑单元：同步徽标 tone 与供应商选项态的映射
//
// 页面上唯一带分支逻辑的部分（其余为声明式 JSX）——先测后写，
// 契约见 syncStatus.test.ts 顶部注释（spec §7.1.3 真值表 / §8 边界 9）。
// ============================================================================

import type { ProjectRouteInfo, ProjectSyncStatus } from "@/types/projectRouting";

/** 同步徽标 tone：ok=正常（绿）/ warn=不同步（黄）/ info=孤儿行提示（蓝） */
export type SyncBadgeTone = "ok" | "warn" | "info";

/** 供应商下拉当前值的三种形态 */
export type ProviderOptionKind = "default" | "bound" | "invalid";

/** sync_status 三态 → 徽标 tone */
export function syncBadgeTone(status: ProjectSyncStatus): SyncBadgeTone {
  switch (status) {
    case "synced":
      return "ok";
    case "out_of_sync":
      return "warn";
    case "orphan_header":
      return "info";
  }
}

/** 绑定信息 → 供应商下拉选项态（invalid 不静默清空展示，用户可主动改选） */
export function providerOptionKind(
  info: Pick<ProjectRouteInfo, "boundProviderId" | "boundProviderValid">,
): ProviderOptionKind {
  if (!info.boundProviderId) {
    return "default";
  }
  return info.boundProviderValid ? "bound" : "invalid";
}
