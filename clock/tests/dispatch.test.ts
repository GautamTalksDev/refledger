import { describe, expect, it, vi } from "vitest";

import {
  buildDispatchRequest,
  CANARY_CRON,
  CANARY_DISPATCH_URL,
  DISPATCH_BODY,
  DISPATCH_URL,
  dispatchPoll,
  dispatchWorkflow,
  OPERATIONS_CONTACT,
  POLL_CRON,
  POLL_DISPATCH_URL,
  targetForCron,
  USER_AGENT,
  type FetchLike,
} from "../src/index";

const TOKEN = "ghs_test_token_never_log_me_9f3a";

describe("targetForCron", () => {
  it("maps the poll cron to poll", () => {
    expect(targetForCron(POLL_CRON)).toBe("poll");
    expect(targetForCron("2-57/5 * * * *")).toBe("poll");
  });

  it("maps the canary cron to canary", () => {
    expect(targetForCron(CANARY_CRON)).toBe("canary");
    expect(targetForCron("17 */4 * * *")).toBe("canary");
  });

  it("returns null for unknown crons", () => {
    expect(targetForCron("0 * * * *")).toBeNull();
  });
});

describe("buildDispatchRequest", () => {
  it("targets the poll.yml workflow_dispatch endpoint by default", () => {
    const req = buildDispatchRequest(TOKEN);
    expect(req.target).toBe("poll");
    expect(req.url).toBe(DISPATCH_URL);
    expect(req.url).toBe(POLL_DISPATCH_URL);
    expect(req.url).toBe(
      "https://api.github.com/repos/GautamTalksDev/refledger/actions/workflows/poll.yml/dispatches",
    );
    expect(req.method).toBe("POST");
  });

  it("targets the canary.yml workflow_dispatch endpoint", () => {
    const req = buildDispatchRequest(TOKEN, "canary");
    expect(req.target).toBe("canary");
    expect(req.url).toBe(CANARY_DISPATCH_URL);
    expect(req.url).toBe(
      "https://api.github.com/repos/GautamTalksDev/canary/actions/workflows/canary.yml/dispatches",
    );
    expect(JSON.parse(req.body)).toEqual({ ref: "main" });
  });

  it("sets the required GitHub headers and body", () => {
    const req = buildDispatchRequest(TOKEN, "poll");
    expect(req.headers.Authorization).toBe(`Bearer ${TOKEN}`);
    expect(req.headers.Accept).toBe("application/vnd.github+json");
    expect(req.headers["X-GitHub-Api-Version"]).toBe("2022-11-28");
    expect(req.headers["User-Agent"]).toBe(USER_AGENT);
    expect(req.headers["User-Agent"]).toContain(OPERATIONS_CONTACT);
    expect(req.headers["Content-Type"]).toBe("application/json");
    expect(req.body).toBe(DISPATCH_BODY);
    expect(JSON.parse(req.body)).toEqual({ ref: "main" });
  });
});

describe("dispatchWorkflow", () => {
  it("returns on 204 without sleeping (poll)", async () => {
    const fetchImpl = vi.fn<FetchLike>(async () => ({ status: 204 }));
    const sleep = vi.fn(async () => {});
    const logs: string[] = [];
    const result = await dispatchWorkflow(
      TOKEN,
      "poll",
      fetchImpl,
      sleep,
      (m) => logs.push(m),
    );
    expect(result).toEqual({ status: 204, attempts: 1, target: "poll" });
    expect(fetchImpl).toHaveBeenCalledOnce();
    expect(sleep).not.toHaveBeenCalled();
    expect(logs).toEqual(["dispatch target=poll status=204 attempt=1"]);
  });

  it("dispatches canary with the canary URL", async () => {
    const fetchImpl = vi.fn<FetchLike>(async () => ({ status: 204 }));
    const logs: string[] = [];
    const result = await dispatchWorkflow(
      TOKEN,
      "canary",
      fetchImpl,
      async () => {},
      (m) => logs.push(m),
    );
    expect(result).toEqual({ status: 204, attempts: 1, target: "canary" });
    const expected = buildDispatchRequest(TOKEN, "canary");
    expect(fetchImpl).toHaveBeenCalledWith(expected.url, {
      method: expected.method,
      headers: expected.headers,
      body: expected.body,
    });
    expect(logs).toEqual(["dispatch target=canary status=204 attempt=1"]);
  });

  it("retries once after 5 seconds on 503", async () => {
    const fetchImpl = vi
      .fn<FetchLike>()
      .mockResolvedValueOnce({ status: 503 })
      .mockResolvedValueOnce({ status: 204 });
    const sleep = vi.fn(async () => {});
    const logs: string[] = [];
    const result = await dispatchWorkflow(
      TOKEN,
      "canary",
      fetchImpl,
      sleep,
      (m) => logs.push(m),
    );
    expect(result).toEqual({ status: 204, attempts: 2, target: "canary" });
    expect(fetchImpl).toHaveBeenCalledTimes(2);
    expect(sleep).toHaveBeenCalledExactlyOnceWith(5000);
    expect(logs).toEqual([
      "dispatch target=canary status=503 attempt=1",
      "dispatch target=canary status=204 attempt=2",
    ]);
  });

  it("retries once on network error", async () => {
    const fetchImpl = vi
      .fn<FetchLike>()
      .mockRejectedValueOnce(new Error("connect reset"))
      .mockResolvedValueOnce({ status: 204 });
    const sleep = vi.fn(async () => {});
    const logs: string[] = [];
    const result = await dispatchWorkflow(
      TOKEN,
      "poll",
      fetchImpl,
      sleep,
      (m) => logs.push(m),
    );
    expect(result).toEqual({ status: 204, attempts: 2, target: "poll" });
    expect(sleep).toHaveBeenCalledExactlyOnceWith(5000);
    expect(logs).toEqual([
      "dispatch target=poll network_error attempt=1",
      "dispatch target=poll status=204 attempt=2",
    ]);
  });

  it("does not retry on 401", async () => {
    const fetchImpl = vi.fn<FetchLike>(async () => ({ status: 401 }));
    const sleep = vi.fn(async () => {});
    const logs: string[] = [];
    const result = await dispatchWorkflow(
      TOKEN,
      "poll",
      fetchImpl,
      sleep,
      (m) => logs.push(m),
    );
    expect(result).toEqual({ status: 401, attempts: 1, target: "poll" });
    expect(fetchImpl).toHaveBeenCalledOnce();
    expect(sleep).not.toHaveBeenCalled();
  });

  it("does not retry on 404", async () => {
    const fetchImpl = vi.fn<FetchLike>(async () => ({ status: 404 }));
    const sleep = vi.fn(async () => {});
    const result = await dispatchWorkflow(
      TOKEN,
      "canary",
      fetchImpl,
      sleep,
      () => {},
    );
    expect(result).toEqual({ status: 404, attempts: 1, target: "canary" });
    expect(fetchImpl).toHaveBeenCalledOnce();
    expect(sleep).not.toHaveBeenCalled();
  });

  it("never logs the token or Authorization header", async () => {
    const fetchImpl = vi
      .fn<FetchLike>()
      .mockRejectedValueOnce(new Error(`auth leaked ${TOKEN}`))
      .mockResolvedValueOnce({ status: 503 });
    const sleep = vi.fn(async () => {});
    const logs: string[] = [];
    await dispatchWorkflow(TOKEN, "canary", fetchImpl, sleep, (m) =>
      logs.push(m),
    );
    const joined = logs.join("\n");
    expect(joined).not.toContain(TOKEN);
    expect(joined.toLowerCase()).not.toContain("authorization");
    expect(joined.toLowerCase()).not.toContain("bearer");
    expect(joined).not.toMatch(/ghs_/);
  });
});

describe("dispatchPoll", () => {
  it("is a poll-target wrapper", async () => {
    const fetchImpl = vi.fn<FetchLike>(async () => ({ status: 204 }));
    const result = await dispatchPoll(TOKEN, fetchImpl, async () => {}, () => {});
    expect(result).toEqual({ status: 204, attempts: 1 });
    expect(fetchImpl.mock.calls[0]![0]).toBe(POLL_DISPATCH_URL);
  });
});

describe("scheduled handler", () => {
  it("dispatches poll on the poll cron", async () => {
    const fetchImpl = vi.fn<FetchLike>(async () => ({ status: 204 }));
    const original = globalThis.fetch;
    (globalThis as { fetch: FetchLike }).fetch = fetchImpl;
    const logs: string[] = [];
    const logSpy = vi.spyOn(console, "log").mockImplementation((m: string) => {
      logs.push(String(m));
    });
    try {
      const mod = await import("../src/index");
      await mod.default.scheduled(
        { cron: POLL_CRON, scheduledTime: 0, noRetry() {} } as ScheduledController,
        { DISPATCH_TOKEN: TOKEN },
        {} as ExecutionContext,
      );
      expect(fetchImpl.mock.calls[0]![0]).toBe(POLL_DISPATCH_URL);
      expect(logs.some((l) => l.includes("target=poll"))).toBe(true);
    } finally {
      globalThis.fetch = original;
      logSpy.mockRestore();
    }
  });

  it("dispatches canary on the canary cron", async () => {
    const fetchImpl = vi.fn<FetchLike>(async () => ({ status: 204 }));
    const original = globalThis.fetch;
    (globalThis as { fetch: FetchLike }).fetch = fetchImpl;
    const logs: string[] = [];
    const logSpy = vi.spyOn(console, "log").mockImplementation((m: string) => {
      logs.push(String(m));
    });
    try {
      const mod = await import("../src/index");
      await mod.default.scheduled(
        {
          cron: CANARY_CRON,
          scheduledTime: 0,
          noRetry() {},
        } as ScheduledController,
        { DISPATCH_TOKEN: TOKEN },
        {} as ExecutionContext,
      );
      expect(fetchImpl.mock.calls[0]![0]).toBe(CANARY_DISPATCH_URL);
      expect(logs.some((l) => l.includes("target=canary"))).toBe(true);
    } finally {
      globalThis.fetch = original;
      logSpy.mockRestore();
    }
  });

  it("logs and skips unknown crons", async () => {
    const fetchImpl = vi.fn<FetchLike>(async () => ({ status: 204 }));
    const original = globalThis.fetch;
    (globalThis as { fetch: FetchLike }).fetch = fetchImpl;
    const logs: string[] = [];
    const logSpy = vi.spyOn(console, "log").mockImplementation((m: string) => {
      logs.push(String(m));
    });
    try {
      const mod = await import("../src/index");
      await mod.default.scheduled(
        {
          cron: "0 0 * * *",
          scheduledTime: 0,
          noRetry() {},
        } as ScheduledController,
        { DISPATCH_TOKEN: TOKEN },
        {} as ExecutionContext,
      );
      expect(fetchImpl).not.toHaveBeenCalled();
      expect(logs.some((l) => l.includes("unknown_cron="))).toBe(true);
    } finally {
      globalThis.fetch = original;
      logSpy.mockRestore();
    }
  });
});

describe("fetch handler", () => {
  it("always returns 404", async () => {
    const mod = await import("../src/index");
    const res = await mod.default.fetch();
    expect(res.status).toBe(404);
  });
});
