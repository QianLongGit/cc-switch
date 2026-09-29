import { describe, expect, it } from "vitest";

// ============================================================================
// syncStatus 纯函数测试：项目路由页面上唯一带分支逻辑的单元
//
// 契约（对齐 T11 计划与 spec §7.1.3 真值表 / §8 边界 9）：
//   1. syncBadgeTone 三态映射：synced→ok / out_of_sync→warn / orphan_header→info；
//   2. providerOptionKind：无绑定→default；绑定有效→bound；绑定供应商已删
//      （boundProviderValid=false）→invalid——不静默清空绑定展示。
// ============================================================================

import { providerOptionKind, syncBadgeTone } from "./syncStatus";
import type {
  ProjectRouteInfo,
  ProjectSyncStatus,
} from "@/types/projectRouting";

// ProjectRouteInfo 字段众多，测试只关心绑定维度——Pick 收窄构造入参
type BindingCarrier = Pick<
  ProjectRouteInfo,
  "boundProviderId" | "boundProviderValid"
>;

const carrier = (
  boundProviderId?: string,
  boundProviderValid = true,
): BindingCarrier => ({ boundProviderId, boundProviderValid });

describe("syncBadgeTone", () => {
  it("synced → ok", () => {
    expect(syncBadgeTone("synced")).toBe("ok");
  });

  it("out_of_sync → warn", () => {
    expect(syncBadgeTone("out_of_sync")).toBe("warn");
  });

  it("orphan_header → info", () => {
    expect(syncBadgeTone("orphan_header")).toBe("info");
  });

  it("三态全覆盖：每个 ProjectSyncStatus 都有确定 tone", () => {
    const all: ProjectSyncStatus[] = ["synced", "out_of_sync", "orphan_header"];
    for (const status of all) {
      expect(["ok", "warn", "info"]).toContain(syncBadgeTone(status));
    }
  });
});

describe("providerOptionKind", () => {
  it("无绑定（boundProviderId 缺省）→ default", () => {
    expect(providerOptionKind(carrier(undefined))).toBe("default");
  });

  it("空串 id 同样视为无绑定 → default", () => {
    expect(providerOptionKind(carrier(""))).toBe("default");
  });

  it("绑定且有效 → bound", () => {
    expect(providerOptionKind(carrier("p-1", true))).toBe("bound");
  });

  it("绑定供应商已删（boundProviderValid=false）→ invalid", () => {
    expect(providerOptionKind(carrier("p-deleted", false))).toBe("invalid");
  });
});
