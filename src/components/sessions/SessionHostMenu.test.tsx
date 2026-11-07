import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import type { SshHostConfig } from "@/types";
import { SessionHostMenu } from "./SessionHostMenu";

vi.mock("@/lib/api", () => ({
  sessionsApi: {
    checkSshAvailability: vi.fn(async () => true),
  },
}));

import { sessionsApi } from "@/lib/api";

const HOSTS: SshHostConfig[] = [
  { id: "h1", label: "build-box", host: "192.168.1.10", user: "dev" },
  { id: "h2", label: "NAS", host: "nas.local" },
];

const renderMenu = (
  props: Partial<React.ComponentProps<typeof SessionHostMenu>> = {},
) => {
  const onSelect = vi.fn();
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const utils = render(
    <QueryClientProvider client={client}>
      <SessionHostMenu
        hostId={null}
        sshHosts={HOSTS}
        onSelect={onSelect}
        {...props}
      />
    </QueryClientProvider>,
  );
  return { onSelect, ...utils };
};

describe("SessionHostMenu", () => {
  it("没有配置 SSH 主机时整个控件不渲染", () => {
    vi.mocked(sessionsApi.checkSshAvailability).mockResolvedValue(true);
    const { container } = renderMenu({ sshHosts: [] });
    expect(container).toBeEmptyDOMElement();
  });

  it("sshHosts 未加载时（undefined）同样不渲染", () => {
    const { container } = renderMenu({ sshHosts: undefined });
    expect(container).toBeEmptyDOMElement();
  });

  it("默认选中本机；打开菜单可选远程主机", async () => {
    vi.mocked(sessionsApi.checkSshAvailability).mockResolvedValue(true);
    const user = userEvent.setup();
    const { onSelect } = renderMenu();

    // trigger 上显示「本机」
    expect(screen.getByRole("button", { name: /本机/ })).toBeEnabled();

    await user.click(screen.getByRole("button", { name: /本机/ }));
    expect(screen.getAllByRole("menuitemradio")).toHaveLength(3);

    await user.click(screen.getByRole("menuitemradio", { name: /build-box/ }));
    expect(onSelect).toHaveBeenCalledWith("h1");
  });

  it("菜单里的「本机」项回传 null", async () => {
    vi.mocked(sessionsApi.checkSshAvailability).mockResolvedValue(true);
    const user = userEvent.setup();
    const { onSelect } = renderMenu({ hostId: "h1" });

    await user.click(screen.getByRole("button", { name: /build-box/ }));
    await user.click(screen.getByRole("menuitemradio", { name: /本机/ }));
    expect(onSelect).toHaveBeenCalledWith(null);
  });

  it("选中的远程主机不在列表里时（配置被删）trigger 退回显示本机", () => {
    renderMenu({ hostId: "gone" });
    expect(screen.getByRole("button", { name: /本机/ })).toBeEnabled();
  });

  it("checkSshAvailability 为 false 时禁用入口", async () => {
    vi.mocked(sessionsApi.checkSshAvailability).mockResolvedValue(false);
    renderMenu();

    // 可用性结果回来前不禁用，查到 false 后降级。
    // 重新查询而不是持有元素引用：disabled 变化会重建按钮节点，旧引用不会更新
    await waitFor(() =>
      expect(screen.getByRole("button", { name: /本机/ })).toBeDisabled(),
    );
  });
});
