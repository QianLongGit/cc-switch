import { describe, expect, it } from "vitest";
import type { SessionMeta } from "@/types";
import { isRemoteSession } from "./utils";

const meta = (sourcePath?: string): SessionMeta => ({
  providerId: "claude",
  sessionId: "s1",
  sourcePath,
});

describe("isRemoteSession", () => {
  it("sourcePath 以 ssh: 开头的是远程会话", () => {
    expect(isRemoteSession(meta("ssh:h1:/home/dev/.claude/projects/x.jsonl"))).toBe(
      true,
    );
  });

  it("本机绝对路径不是远程会话", () => {
    expect(isRemoteSession(meta("/Users/dev/.claude/projects/x.jsonl"))).toBe(
      false,
    );
  });

  it("没有 sourcePath 视为本机", () => {
    expect(isRemoteSession(meta(undefined))).toBe(false);
  });
});
