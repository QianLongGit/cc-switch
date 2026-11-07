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

vi.mock("@/lib/api", () => ({
  sessionsApi: {
    listSshConfigHosts: vi.fn(async () => [] as SshConfigHost[]),
    testSshHost: vi.fn(async () => true),
  },
}));

import { sessionsApi } from "@/lib/api";

// ============================================================================
// SSH 主机管理分区：zod schema / 导入匹配归一 / 组件渲染三层。
// 组件测试从简（库内先例形态）：列表、空态、添加对话框开关、导入去重禁用。
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

  it("user 参与匹配；host 大小写与首尾空白归一", () => {
    const existing: SshHostConfig[] = [
      { id: "h1", label: "box", host: "Example.COM ", user: " Dev " },
    ];
    expect(
      isSshConfigHostImported(
        { alias: "box", hostName: "example.com", user: "dev" },
        existing,
      ),
    ).toBe(true);
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

    // 确认框先出现（含缓存保留提示），未确认前不提交
    const dialog = screen.getByRole("alertdialog");
    expect(within(dialog).getByText(/远程会话缓存/)).toBeInTheDocument();
    expect(onAutoSave).not.toHaveBeenCalled();

    await user.click(within(dialog).getByRole("button", { name: /删除主机/ }));
    await waitFor(() =>
      expect(onAutoSave).toHaveBeenCalledWith({
        sshHosts: [{ id: "h2", label: "NAS", host: "nas.local" }],
      }),
    );
  });
});
