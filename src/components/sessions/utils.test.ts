import { describe, expect, it } from "vitest";
import type { SessionMeta, SshHostConfig } from "@/types";
import { isRemoteSession, normalizeHostId } from "./utils";

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

// ─── 已删主机的 hostId 归一（数据层：归一为 null 后 useSessionsQuery 收到 undefined） ───

const HOSTS: SshHostConfig[] = [
  { id: "h1", label: "build-box", host: "192.168.1.10" },
];

describe("normalizeHostId", () => {
  it("hostId 仍在设置里时原样保留", () => {
    expect(normalizeHostId("h1", HOSTS)).toBe("h1");
  });

  it("选中的主机被从设置里删掉时归一回本机（null）", () => {
    expect(normalizeHostId("gone", HOSTS)).toBeNull();
  });

  it("sshHosts 还没加载（undefined）时远程 hostId 同样归一为本机", () => {
    expect(normalizeHostId("h1", undefined)).toBeNull();
  });

  it("hostId 本来就是本机（null）时保持 null", () => {
    expect(normalizeHostId(null, HOSTS)).toBeNull();
  });
});
