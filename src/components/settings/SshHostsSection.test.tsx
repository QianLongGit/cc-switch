import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { SshConfigHost, SshHostConfig } from "@/types";
import type { SettingsFormState } from "@/hooks/useSettings";
import {
  isSshConfigHostImported,
  SshHostsSection,
  sshHostFormSchema,
} from "./SshHostsSection";

// purge 走 usageApi（Task B 删除主机连带清理）；toast mock 供警告断言
const toastWarning = vi.hoisted(() => vi.fn());
vi.mock("@/lib/toast", () => ({
  toast: { warning: toastWarning, success: vi.fn(), error: vi.fn() },
}));

vi.mock("@/lib/api", () => ({
  sessionsApi: {
    listSshConfigHosts: vi.fn(async () => [] as SshConfigHost[]),
    testSshHost: vi.fn(async () => true),
  },
  usageApi: {
    purgeRemoteHostData: vi.fn(async () => ({
      deletedDetails: 0,
      deletedRollups: 0,
      deletedCursors: 0,
      deletedDedup: 0,
    })),
  },
}));

import { sessionsApi, usageApi } from "@/lib/api";

// ============================================================================
// SSH 主机管理分区：zod schema / 导入匹配归一 / 组件渲染三层。
// 组件测试从简（库内先例形态）：列表、空态、添加对话框开关、导入去重禁用；
// 另有一组样式类名断言，守护对话框与设置页表单惯例的对齐（Task 1）。
// ============================================================================

const HOSTS: SshHostConfig[] = [
  {
    id: "h1",
    label: "build-box",
    host: "192.168.1.10",
    user: "dev",
    port: 2222,
  },
  { id: "h2", label: "NAS", host: "nas.local" },
];

const baseSettings = (sshHosts?: SshHostConfig[]) =>
  ({ sshHosts, language: "zh" }) as unknown as SettingsFormState;

const renderSection = (sshHosts?: SshHostConfig[]) => {
  const onAutoSave = vi.fn(async () => true);
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const utils = render(
    <QueryClientProvider client={client}>
      <SshHostsSection
        settings={baseSettings(sshHosts)}
        onAutoSave={onAutoSave}
      />
    </QueryClientProvider>,
  );
  return { onAutoSave, ...utils };
};

// ─── zod schema ─────────────────────────────────────────────────────────────

describe("sshHostFormSchema", () => {
  const valid = { id: "a1b2c3", label: "box", host: "example.com" };

  it("port 越界拒绝：0 与 70000", () => {
    expect(sshHostFormSchema.safeParse({ ...valid, port: 0 }).success).toBe(
      false,
    );
    expect(sshHostFormSchema.safeParse({ ...valid, port: 70000 }).success).toBe(
      false,
    );
  });

  it("port 合法值通过；字符串输入（表单原始值）同样归一通过；缺省通过", () => {
    expect(sshHostFormSchema.safeParse({ ...valid, port: 22 }).success).toBe(
      true,
    );
    expect(
      sshHostFormSchema.safeParse({ ...valid, port: "2202" }).success,
    ).toBe(true);
    expect(sshHostFormSchema.safeParse(valid).success).toBe(true);
  });

  it("label / host 空值拒绝（纯空白同样拒绝）", () => {
    expect(sshHostFormSchema.safeParse({ ...valid, label: "" }).success).toBe(
      false,
    );
    expect(
      sshHostFormSchema.safeParse({ ...valid, label: "   " }).success,
    ).toBe(false);
    expect(sshHostFormSchema.safeParse({ ...valid, host: "" }).success).toBe(
      false,
    );
  });

  it("id 含 / 拒绝（ssh: source_path 往返保护）", () => {
    expect(
      sshHostFormSchema.safeParse({ ...valid, id: "bad/id" }).success,
    ).toBe(false);
  });
});

// ─── 导入匹配归一（纯函数） ──────────────────────────────────────────────────

describe("isSshConfigHostImported", () => {
  it("port 缺省归一 22：config 条目 port 22 与已存无 port 视为同一主机", () => {
    const existing: SshHostConfig[] = [
      { id: "h1", label: "nas", host: "nas.local" },
    ];
    expect(
      isSshConfigHostImported(
        { alias: "nas", hostName: "nas.local", port: 22 },
        existing,
      ),
    ).toBe(true);
    expect(
      isSshConfigHostImported(
        { alias: "nas", hostName: "nas.local", port: 2222 },
        existing,
      ),
    ).toBe(false);
  });

  it("hostName 缺省时用 alias 参与匹配", () => {
    const existing: SshHostConfig[] = [
      { id: "h1", label: "box", host: "example.com" },
    ];
    expect(isSshConfigHostImported({ alias: "example.com" }, existing)).toBe(
      true,
    );
  });

  it("user 参与匹配且大小写敏感；host 大小写与首尾空白归一", () => {
    const existing: SshHostConfig[] = [
      { id: "h1", label: "box", host: "Example.COM ", user: " Dev " },
    ];
    // host 归一（大小写 / 空白），user trim 后完全一致 → 同一主机
    expect(
      isSshConfigHostImported(
        { alias: "box", hostName: "example.com", user: "Dev" },
        existing,
      ),
    ).toBe(true);
    // ssh 用户名大小写敏感：dev ≠ Dev，不判已导入
    expect(
      isSshConfigHostImported(
        { alias: "box", hostName: "example.com", user: "dev" },
        existing,
      ),
    ).toBe(false);
    // user 缺省与显式 user 同样是不同身份
    expect(
      isSshConfigHostImported(
        { alias: "box", hostName: "example.com" },
        existing,
      ),
    ).toBe(false);
  });
});

// ─── 组件渲染 ───────────────────────────────────────────────────────────────

describe("SshHostsSection", () => {
  beforeEach(() => {
    vi.mocked(sessionsApi.listSshConfigHosts).mockResolvedValue([]);
    vi.mocked(sessionsApi.testSshHost).mockResolvedValue(true);
  });

  it("渲染主机列表：label 与 user@host:port 副文案", () => {
    renderSection(HOSTS);
    expect(screen.getByText("build-box")).toBeInTheDocument();
    expect(screen.getByText(/dev@192\.168\.1\.10:2222/)).toBeInTheDocument();
    expect(screen.getByText("NAS")).toBeInTheDocument();
    expect(screen.getByText(/nas\.local/)).toBeInTheDocument();
  });

  it("空态：没有主机时显示引导文案而非空白卡片", () => {
    renderSection([]);
    expect(screen.getByText(/还没有 SSH 主机/)).toBeInTheDocument();
  });

  it("点「添加主机」打开双 tab 对话框", async () => {
    const user = userEvent.setup();
    renderSection([]);
    await user.click(screen.getByRole("button", { name: /添加主机/ }));
    expect(
      screen.getByRole("tab", { name: /从 SSH Config 导入/ }),
    ).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /手动添加/ })).toBeInTheDocument();
  });

  it("导入列表渲染 ssh config 条目；已导入条目禁用并标记", async () => {
    vi.mocked(sessionsApi.listSshConfigHosts).mockResolvedValue([
      { alias: "build-box", hostName: "192.168.1.10", user: "dev", port: 2222 },
      { alias: "staging", hostName: "stg.example.com" },
    ]);
    const user = userEvent.setup();
    renderSection(HOSTS);
    await user.click(screen.getByRole("button", { name: /添加主机/ }));

    await waitFor(() =>
      expect(screen.getByRole("checkbox", { name: /staging/ })).toBeEnabled(),
    );
    // 192.168.1.10:2222:dev 已在 sshHosts 里 → 禁用 + 已导入标记
    const imported = screen.getByRole("checkbox", { name: /build-box/ });
    expect(imported).toBeDisabled();
    expect(screen.getAllByText(/已导入/).length).toBeGreaterThan(0);
  });

  it("测试连接成功后行内显示成功状态", async () => {
    const user = userEvent.setup();
    renderSection(HOSTS);
    await user.click(screen.getAllByRole("button", { name: /测试连接/ })[0]);

    await waitFor(() => {
      expect(sessionsApi.testSshHost).toHaveBeenCalledWith(
        expect.objectContaining({ id: "h1" }),
      );
    });
    await waitFor(() =>
      expect(screen.getAllByText(/连接成功/).length).toBeGreaterThan(0),
    );
  });

  it("测试连接失败时行内显示失败状态（后端分类文案挂在悬停提示上）", async () => {
    vi.mocked(sessionsApi.testSshHost).mockRejectedValue(
      new Error("认证失败：私钥被拒绝"),
    );
    const user = userEvent.setup();
    renderSection(HOSTS);
    await user.click(screen.getAllByRole("button", { name: /测试连接/ })[0]);

    await waitFor(() =>
      expect(screen.getAllByText(/连接失败/).length).toBeGreaterThan(0),
    );
  });

  it("删除走确认对话框：确认后才提交新 sshHosts", async () => {
    const user = userEvent.setup();
    const { onAutoSave } = renderSection(HOSTS);
    await user.click(screen.getAllByRole("button", { name: /^删除$/ })[0]);

    // 确认框先出现（含连带清理提示），未确认前不 purge 不提交
    const dialog = screen.getByRole("alertdialog");
    expect(within(dialog).getByText(/会话缓存/)).toBeInTheDocument();
    expect(usageApi.purgeRemoteHostData).not.toHaveBeenCalled();
    expect(onAutoSave).not.toHaveBeenCalled();

    await user.click(within(dialog).getByRole("button", { name: /删除主机/ }));
    await waitFor(() =>
      expect(onAutoSave).toHaveBeenCalledWith({
        sshHosts: [{ id: "h2", label: "NAS", host: "nas.local" }],
      }),
    );
  });
});

// ─── 删除主机连带清理（Task B：purge → save_settings 顺序锁定） ─────────────

describe("SshHostsSection 删除主机连带清理", () => {
  beforeEach(() => {
    vi.mocked(usageApi.purgeRemoteHostData).mockResolvedValue({
      deletedDetails: 7,
      deletedRollups: 4,
      deletedCursors: 3,
      deletedDedup: 2,
    });
    toastWarning.mockClear();
  });

  /** 走完删除确认流程，返回按发生顺序记录的调用日志（purge / save 各记一笔） */
  const deleteFirstHost = async () => {
    const calls: string[] = [];
    // 包装而非覆盖用例先设好的实现（resolve / reject 都照跑），只加调用记录
    const purge = vi.mocked(usageApi.purgeRemoteHostData);
    const impl =
      purge.getMockImplementation() ??
      (async () => ({
        deletedDetails: 0,
        deletedRollups: 0,
        deletedCursors: 0,
        deletedDedup: 0,
      }));
    purge.mockImplementation(async (id: string) => {
      calls.push(`purge:${id}`);
      return impl(id);
    });
    const onAutoSave = vi.fn(async (updates: Partial<SettingsFormState>) => {
      calls.push(
        `save:${
          (updates as { sshHosts?: SshHostConfig[] }).sshHosts
            ?.map((h) => h.id)
            .join(",") ?? ""
        }`,
      );
      return true;
    });
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    render(
      <QueryClientProvider client={client}>
        <SshHostsSection
          settings={baseSettings(HOSTS)}
          onAutoSave={onAutoSave}
        />
      </QueryClientProvider>,
    );

    const user = userEvent.setup();
    await user.click(screen.getAllByRole("button", { name: /^删除$/ })[0]);
    const dialog = screen.getByRole("alertdialog");
    await user.click(within(dialog).getByRole("button", { name: /删除主机/ }));
    await waitFor(() => expect(onAutoSave).toHaveBeenCalled());
    return calls;
  };

  it("确认删除：先 purge_remote_host_data(hostId) 再 save_settings（顺序锁定）", async () => {
    const calls = await deleteFirstHost();
    // purge 必须先行：后端按 settings 校验主机存在，save 先行会让 purge 拒绝执行
    expect(calls).toEqual(["purge:h1", "save:h2"]);
  });

  it("purge 失败也继续删除主机：save_settings 照常提交，仅 toast 警告", async () => {
    vi.mocked(usageApi.purgeRemoteHostData).mockRejectedValue(
      new Error("数据库被占用"),
    );
    const calls = await deleteFirstHost();

    // purge 仍先于 save 发生（失败被吞），删除主机的意图不被清理失败阻断
    expect(calls).toEqual(["purge:h1", "save:h2"]);
    await waitFor(() =>
      expect(toastWarning).toHaveBeenCalledWith(
        expect.stringContaining("数据清理未完全成功"),
      ),
    );
  });
});

// ─── 表单样式对齐（与设置页表单惯例统一） ───────────────────────────────────
//
// DialogContent 本体无 padding、Header/Footer 自带 px-6：中间内容必须自己
// 包 px-6 才与标题对齐。断言用 closest 定位（radix Tabs Root 会多渲染一层
// 无类名 div，不能假设直接父子关系）。

describe("SshHostsSection 表单样式对齐", () => {
  beforeEach(() => {
    vi.mocked(sessionsApi.listSshConfigHosts).mockResolvedValue([]);
  });

  it("添加对话框：内容区包 px-6 与 Header 对齐；宽度收窄至 max-w-md", async () => {
    const user = userEvent.setup();
    renderSection([]);
    await user.click(screen.getByRole("button", { name: /添加主机/ }));

    const dialog = screen.getByRole("dialog");
    expect(dialog.className).toContain("max-w-md");
    expect(dialog.className).not.toContain("max-w-lg");

    // tablist 向上最近的 px-6 容器即 body 包裹层：只负责左右对齐，
    // 不带 pb——底距交给各 tab 内 DialogFooter 的 pb-5，避免叠加
    const body = screen.getByRole("tablist").closest("div.px-6");
    expect(body).not.toBeNull();
    expect(body?.className).toContain("px-6");
    expect(body?.className).not.toContain("pb-5");
  });

  it("手动表单：user/port 双列为 1fr + 140px 窄列；底部按钮区为 DialogFooter", async () => {
    const user = userEvent.setup();
    renderSection([]);
    await user.click(screen.getByRole("button", { name: /添加主机/ }));
    await user.click(screen.getByRole("tab", { name: "手动添加" }));

    // 端口输入框所在的 grid 行：端口窄列先例（CodexFormFields.tsx:1735）
    const gridRow = screen.getByPlaceholderText("22").closest("div.grid");
    expect(gridRow?.className).toContain("grid-cols-[1fr_140px]");

    // 按钮区迁入 DialogFooter：结构特征类（px-0 抵消外层 px-6，边距靠 pb-5/pt-3）
    const footer = screen.getByRole("button", { name: "添加" }).parentElement;
    expect(footer?.className).toContain("justify-end");
    expect(footer?.className).toContain("pb-5");
    expect(footer?.className).toContain("pt-3");
  });

  it("编辑对话框：表单区包 px-6；宽度收窄至 max-w-md", async () => {
    const user = userEvent.setup();
    renderSection(HOSTS);
    await user.click(screen.getAllByRole("button", { name: "编辑" })[0]);

    const dialog = screen.getByRole("dialog");
    expect(dialog.className).toContain("max-w-md");
    expect(dialog.className).not.toContain("max-w-lg");

    const form = screen.getByPlaceholderText("如：build-box").closest("form");
    expect(form?.closest("div.px-6")).not.toBeNull();
  });

  it("导入列表行密度收紧至 py-1.5", async () => {
    vi.mocked(sessionsApi.listSshConfigHosts).mockResolvedValue([
      { alias: "staging", hostName: "stg.example.com" },
    ]);
    const user = userEvent.setup();
    renderSection([]);
    await user.click(screen.getByRole("button", { name: /添加主机/ }));

    const checkbox = await screen.findByRole("checkbox", { name: "staging" });
    expect(checkbox.closest("label")?.className).toContain("py-1.5");
  });
});
