import { describe, it, expect } from "vitest";
import { SETTINGS_ITEMS } from "./Sidebar";
import { SETTINGS_SECTIONS } from "@/lib/navigation";

// ─── 设置目录一致性 ──────────────────────────────────────────────────────────
// v4.1.0 教训：SSH 主机功能改了 navigation.ts 与 SettingsPage，漏了侧栏数组，
// 导致设置侧栏看不到入口。此测试锁定两个数据源不再漂移。

describe("SETTINGS_ITEMS 与 SETTINGS_SECTIONS 一致性", () => {
  it("侧栏设置目录与导航分组同序同集", () => {
    expect(SETTINGS_ITEMS.map((item) => item.section)).toEqual(SETTINGS_SECTIONS);
  });

  it("设置目录包含 SSH 主机入口", () => {
    expect(SETTINGS_ITEMS.map((item) => item.section)).toContain("sshHosts");
  });
});
