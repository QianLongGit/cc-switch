import { render, screen } from "@testing-library/react";
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

const renderHeader = (launchTerminal: string | null) =>
  render(
    <SessionReaderHeader
      session={SESSION}
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
});
