import { describe, it, expect, vi, beforeEach } from "vitest";
import { sessionsApi } from "./sessions";

const invoke = vi.hoisted(() => vi.fn().mockResolvedValue([]));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

beforeEach(() => {
  invoke.mockClear();
});

describe("sessionsApi ssh bindings", () => {
  it("lists local sessions with hostId null", async () => {
    await sessionsApi.list();
    expect(invoke).toHaveBeenCalledWith("list_sessions", { hostId: null });
  });

  it("lists remote sessions with explicit hostId", async () => {
    await sessionsApi.list("h1");
    expect(invoke).toHaveBeenCalledWith("list_sessions", { hostId: "h1" });
  });

  it("loads ssh config hosts", async () => {
    await sessionsApi.listSshConfigHosts();
    expect(invoke).toHaveBeenCalledWith("list_ssh_config_hosts");
  });

  it("tests an ssh host passing the config through", async () => {
    const host = {
      id: "h1",
      label: "box",
      host: "example.com",
      user: "dev",
      port: 22,
      identityFile: "~/.ssh/id_ed25519",
    };
    await sessionsApi.testSshHost(host);
    expect(invoke).toHaveBeenCalledWith("test_ssh_host", { host });
  });

  it("checks local ssh binary availability", async () => {
    await sessionsApi.checkSshAvailability();
    expect(invoke).toHaveBeenCalledWith("check_ssh_availability");
  });
});
