import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
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

describe("SessionHostMenu 悬停提示（triggerHint）", () => {
  // 修复回归：提示浮层(z-130)默认出现在触发器正下方，与下拉列表(z-100)同侧，
  // 展开菜单时若仍渲染会盖住条目、拦截点击。展开期间必须整体不渲染。
  it("关闭态悬停显示提示；菜单展开期间提示浮层消失，关掉菜单后恢复", async () => {
    vi.mocked(sessionsApi.checkSshAvailability).mockResolvedValue(true);
    const user = userEvent.setup();
    renderMenu({ triggerHint: "切换统计主机" });

    let trigger = screen.getByRole("button", { name: /本机/ });

    // 关闭态：悬停出提示（Radix Portal 渲染在 body 里）
    await user.hover(trigger);
    expect(await screen.findByRole("tooltip")).toHaveTextContent(
      "切换统计主机",
    );

    // 展开菜单：点击本身会关掉提示（Radix onPointerDown），菜单条目照常可选
    await user.click(trigger);
    expect(screen.getAllByRole("menuitemradio")).toHaveLength(3);

    // 真机遮挡的根源：菜单开着时指针仍在触发器上，一次微动就让提示经 0ms
    // 延迟重新弹出，盖住列表条目。展开期间必须整体不渲染提示。
    // （模态菜单会把 body 置 pointer-events:none，userEvent 发不进去，
    //   用 fireEvent 派发等价的指针微动；Radix HideOthers 会把触发器与
    //   tooltip portal 都 aria-hidden，重查与断言都要 hidden:true；
    //   修复后 span 树随展开重建，必须用重查的节点发事件才有效）
    trigger = screen.getByRole("button", { name: /本机/, hidden: true });
    fireEvent.pointerLeave(trigger);
    fireEvent.pointerMove(trigger);
    // 0ms 打开计时器走宏任务：等一拍再断言，避免误判「尚未弹出」。
    // tooltip 的 portal 同样被 aria-hidden，查询必须 hidden:true 才能抓到回归
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 50));
    });
    expect(screen.queryByRole("tooltip", { hidden: true })).toBeNull();

    // 关菜单回焦不弹提示（keepClosedUnlessKeyboard）；指针再移回来才重新显示
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("tooltip", { hidden: true })).toBeNull();
    trigger = screen.getByRole("button", { name: /本机/ });
    await user.unhover(trigger);
    await user.hover(trigger);
    expect(await screen.findByRole("tooltip")).toHaveTextContent(
      "切换统计主机",
    );
  });

  it("ssh 不可用的禁用说明优先于 triggerHint", async () => {
    vi.mocked(sessionsApi.checkSshAvailability).mockResolvedValue(false);
    const user = userEvent.setup();
    renderMenu({ triggerHint: "切换统计主机" });

    // disabled 变化会重建按钮节点：断言用重新查询的引用（同既有用例）
    await waitFor(() =>
      expect(screen.getByRole("button", { name: /本机/ })).toBeDisabled(),
    );

    // disabled 按钮收不到鼠标事件，悬停落在包裹的 span（真正的 tooltip trigger）上
    const tipTrigger = screen
      .getByRole("button", { name: /本机/ })
      .closest("span");
    expect(tipTrigger).not.toBeNull();
    await user.hover(tipTrigger!);
    expect(await screen.findByRole("tooltip")).toHaveTextContent(
      /未检测到 ssh/,
    );
    expect(screen.queryByRole("tooltip")).not.toHaveTextContent("切换统计主机");
  });
});

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
