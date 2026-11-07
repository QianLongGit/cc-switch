import { describe, it, expect } from "vitest";
import { sessionKeys } from "./sessions";

describe("sessionKeys host 扩维", () => {
  it("list() 默认本机 key", () => {
    expect(sessionKeys.list()).toEqual(["sessions", "local"]);
  });

  it('list("h1") 按主机分维', () => {
    expect(sessionKeys.list("h1")).toEqual(["sessions", "h1"]);
  });

  it("all 作为前缀形态覆盖所有主机变体", () => {
    expect(sessionKeys.all).toEqual(["sessions"]);
  });

  it("既有消息 / 转录 key 不回归", () => {
    expect(sessionKeys.messages("claude", "/a.jsonl")).toEqual([
      "sessionMessages",
      "claude",
      "/a.jsonl",
    ]);
    expect(sessionKeys.transcript("claude", "/a.jsonl")).toEqual([
      "sessionTranscript",
      "claude",
      "/a.jsonl",
    ]);
  });
});
