// ============================================================================
// 历史记录项目筛选辅助
//
// 选项来源完全取自当前已拉取的日志数据（客户端派生），不单独请求后端：
// 后端 LogFilters.projectDir 为精确匹配，此清单即合法值域。
// ============================================================================

import type { RequestLog } from "@/types/usage";

/**
 * 从请求日志中提取去重后的项目目录清单。
 *
 * - 跳过 projectDir 为 undefined / 空串的行（未识别或 v21 前历史行）；
 * - 去重后按字母序返回，保证下拉选项顺序稳定。
 */
export function uniqueProjectDirs(
  logs: readonly Pick<RequestLog, "projectDir">[],
): string[] {
  const dirs = new Set<string>();
  for (const log of logs) {
    const dir = log.projectDir;
    if (dir) dirs.add(dir);
  }
  return [...dirs].sort();
}
