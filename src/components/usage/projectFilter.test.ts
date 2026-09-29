import { describe, expect, it } from "vitest";

// ============================================================================
// uniqueProjectDirs 纯函数测试：历史记录项目筛选下拉的唯一选项来源
//
// 契约（对齐 T12 计划）：
//   1. 取非空 projectDir（undefined / 空串视为无项目，跳过）；
//   2. 去重后按字母序返回；
//   3. 空输入 → []。
// ============================================================================

import { uniqueProjectDirs } from "./projectFilter";
import type { RequestLog } from "@/types/usage";

// RequestLog 字段众多，测试只关心 projectDir 维度——用 Pick 收窄构造入参，
// 与真实 RequestLog[] 结构兼容（uniqueProjectDirs 按结构化类型接受两者）。
type ProjectDirCarrier = Pick<RequestLog, "projectDir">;

const log = (projectDir?: string): ProjectDirCarrier => ({ projectDir });

describe("uniqueProjectDirs", () => {
  it("空输入返回空数组", () => {
    expect(uniqueProjectDirs([])).toEqual([]);
  });

  it("跳过 projectDir 为 undefined 的行", () => {
    expect(uniqueProjectDirs([log(), log(), log()])).toEqual([]);
  });

  it("空字符串 projectDir 同样跳过", () => {
    expect(uniqueProjectDirs([log(""), log("/Users/dev/Project/cc-switch")])).toEqual([
      "/Users/dev/Project/cc-switch",
    ]);
  });

  it("重复路径去重（保序无关，结果唯一）", () => {
    expect(
      uniqueProjectDirs([
        log("/Users/dev/Project/cc-switch"),
        log("/Users/dev/Project/other"),
        log("/Users/dev/Project/cc-switch"),
      ]),
    ).toEqual(["/Users/dev/Project/cc-switch", "/Users/dev/Project/other"]);
  });

  it("结果按字母序排序", () => {
    expect(
      uniqueProjectDirs([
        log("/Users/dev/Project/zeta"),
        log("/Users/dev/Project/alpha"),
        log("/Users/dev/Project/mid"),
      ]),
    ).toEqual([
      "/Users/dev/Project/alpha",
      "/Users/dev/Project/mid",
      "/Users/dev/Project/zeta",
    ]);
  });

  it("混合输入：undefined + 空串 + 重复 + 多路径", () => {
    expect(
      uniqueProjectDirs([
        log(),
        log("/Users/dev/Project/b"),
        log(""),
        log("/Users/dev/Project/a"),
        log("/Users/dev/Project/b"),
        log(),
        log("/Users/dev/Project/a"),
      ]),
    ).toEqual(["/Users/dev/Project/a", "/Users/dev/Project/b"]);
  });
});
