import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { AGENT_READER_STYLES } from "./agentStyles";
import { ReaderContext } from "./context";
import { PathChip } from "./PathChip";

const LOCAL_PATH = "/Users/dev/project/src/main.ts";

describe("PathChip 的 reveal 形态", () => {
  it("默认（本地会话）渲染「在 Finder 中显示」按钮", () => {
    render(<PathChip path={LOCAL_PATH} />);
    expect(
      screen.getByRole("button", { name: "在 Finder 中显示" }),
    ).toBeInTheDocument();
  });

  it("阅读上下文声明 revealEnabled=false 时不渲染 reveal 按钮，复制入口保留", () => {
    render(
      <ReaderContext.Provider
        value={{
          style: AGENT_READER_STYLES.generic,
          appName: "Claude Code",
          onCopy: () => undefined,
          revealEnabled: false,
        }}
      >
        <PathChip path={LOCAL_PATH} />
      </ReaderContext.Provider>,
    );
    expect(
      screen.queryByRole("button", { name: "在 Finder 中显示" }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: `复制路径: ${LOCAL_PATH}` }),
    ).toBeInTheDocument();
  });
});
