import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { SessionMeta } from "@/types";
import { SessionReaderHeader } from "./SessionReaderHeader";

const noop = () => undefined;

const SESSION: SessionMeta = {
  providerId: "claude",
  sessionId: "abc123",
  title: "修一个 bug",
  lastActiveAt: Date.now(),
  sourcePath: "/Users/dev/.claude/projects/p/abc123.jsonl",
  projectDir: "/Users/dev/project",
  resumeCommand: "claude --resume abc123",
};

/** 远程会话：sourcePath 带 `ssh:` 前缀，恢复命令是完整 ssh 命令 */
const REMOTE_SESSION: SessionMeta = {
  ...SESSION,
  sourcePath: "ssh:host-1/root/proj/abc123.jsonl",
  projectDir: "/remote/proj",
  resumeCommand:
    "ssh -t host-1 'cd '\\''/remote/proj'\\'' && claude --resume abc123'",
};

const renderHeader = (
  launchTerminal: string | null,
  session: SessionMeta = SESSION,
) =>
  render(
    <SessionReaderHeader
      session={session}
      title="修一个 bug"
      appName="Claude Code"
      launchTerminal={launchTerminal}
      hasPrev={false}
      hasNext={false}
      canCopyMarkdown={false}
      includeThinking={false}
      onIncludeThinkingChange={noop}
      onPrev={noop}
      onNext={noop}
      onBack={noop}
      onLaunch={noop}
      onCopy={noop}
      onCopyMarkdown={noop}
      onExportMarkdown={noop}
      onOpenTerminalSettings={noop}
      onReload={noop}
      onDelete={noop}
    />,
  );

/** 打开「更多恢复方式」下拉（cd 项只在这个菜单里渲染） */
const openResumeMenu = async () => {
  const user = userEvent.setup();
  await user.click(screen.getByRole("button", { name: "更多恢复方式" }));
};

describe("SessionReaderHeader 的恢复入口", () => {
  it("launchTerminal 有值时显示「在终端中恢复」", () => {
    renderHeader("Terminal");
    expect(
      screen.getAllByRole("button", { name: "在 Terminal 中恢复" }).length,
    ).toBeGreaterThan(0);
  });

  it("launchTerminal 为 null（非 macOS 或远程会话）时不显示终端恢复按钮，只留复制命令", () => {
    renderHeader(null);
    expect(
      screen.queryByRole("button", { name: /在 .* 中恢复/ }),
    ).not.toBeInTheDocument();
    // 复制恢复命令仍可用
    const copy = screen.getByRole("button", { name: "复制恢复命令" });
    expect(copy).toBeEnabled();
  });

  it("本地会话显示「复制进入目录并恢复」菜单项", async () => {
    renderHeader("Terminal");
    await openResumeMenu();
    expect(
      screen.getByRole("menuitem", { name: /进入目录并恢复/ }),
    ).toBeInTheDocument();
  });

  it("远程会话（sourcePath 为 ssh: 前缀）隐藏「复制进入目录并恢复」，其余恢复方式保留", async () => {
    renderHeader("Terminal", REMOTE_SESSION);
    await openResumeMenu();
    // cd 语义已内嵌在 ssh 恢复命令里，前端不得再拼本机 cd
    expect(
      screen.queryByRole("menuitem", { name: /进入目录并恢复/ }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", { name: /复制恢复命令/ }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("menuitem", { name: "在 Terminal 中恢复" }),
    ).toBeInTheDocument();
  });
});
