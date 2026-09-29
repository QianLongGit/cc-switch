// 项目路由数据层类型：与后端 `commands/project_routing.rs` 的 ProjectRouteInfo
// DTO 逐字段对齐（serde rename_all = "camelCase"，字段名即 IPC 线上格式）

/** 文件同步三态（settings.local.json 与 DB 绑定的一致性） */
export type ProjectSyncStatus = "synced" | "out_of_sync" | "orphan_header";

/** 单条项目路由信息：扫描产物（路径 / basename / 活跃时间 / 活跃会话）合并 DB 绑定与同步状态 */
export interface ProjectRouteInfo {
  /** 项目根目录绝对路径（唯一键） */
  projectPath: string;
  /** 路径 basename，列表主展示名 */
  basename: string;
  /** 最近活跃时间（Unix 秒） */
  lastActiveAt: number;
  /** 是否有活跃会话 */
  hasActiveSession: boolean;
  /** 绑定供应商 id（未绑定时缺省） */
  boundProviderId?: string;
  /** 绑定供应商名称（悬空绑定时缺省） */
  boundProviderName?: string;
  /** false = 绑定的供应商已删（"已失效"态，不静默清空展示） */
  boundProviderValid: boolean;
  /** 同步三态 */
  syncStatus: ProjectSyncStatus;
}
