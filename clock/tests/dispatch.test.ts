import { describe, expect, it, vi } from "vitest";

import {
  buildDispatchRequest,
  DISPATCH_BODY,
  DISPATCH_URL,
  dispatchPoll,
  OPERATIONS_CONTACT,
  USER_AGENT,
  type FetchLike,
} from "../src/index";

const TOKEN = "ghs_test_token_never_log_me_9f3a";

describe("buildDispatchRequest", () => {
  it("targets the poll.yml workflow_dispatch endpoint", () => {
    const req = buildDispatchRequest(TOKEN);
    expect(req.url).toBe(DISPATCH_URL);
    expect(req.url).toBe(
      "https://api.github.com/repos/GautamTalksDev/refledger/actions/workflows/poll.yml/dispatches",
    );
    expect(req.method).toBe("POST");
  });

  it("sets the required GitHub headers and body", () => {
    const req = buildDispatchRequest(TOKEN);
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

describe("dispatchPoll", () => {
  it("returns on 204 without sleeping", async () => {
    const fetchImpl = vi.fn<FetchLike>(async () => ({ status: 204 }));
    const sleep = vi.fn(async () => {});
    const logs: string[] = [];
    const result = await dispatchPoll(TOKEN, fetchImpl, sleep, (m) => logs.push(m));
    expect(result).toEqual({ status: 204, attempts: 1 });
    expect(fetchImpl).toHaveBeenCalledOnce();
    expect(sleep).not.toHaveBeenCalled();
    expect(logs).toEqual(["dispatch status=204 attempt=1"]);
  });

  it("retries once after 5 seconds on 503", async () => {
    const fetchImpl = vi
      .fn<FetchLike>()
      .mockResolvedValueOnce({ status: 503 })
      .mockResolvedValueOnce({ status: 204 });
    const sleep = vi.fn(async () => {});
    const logs: string[] = [];
    const result = await dispatchPoll(TOKEN, fetchImpl, sleep, (m) => logs.push(m));
    expect(result).toEqual({ status: 204, attempts: 2 });
    expect(fetchImpl).toHaveBeenCalledTimes(2);
    expect(sleep).toHaveBeenCalledExactlyOnceWith(5000);
    expect(logs).toEqual([
      "dispatch status=503 attempt=1",
      "dispatch status=204 attempt=2",
    ]);
  });

  it("retries once on network error", async () => {
    const fetchImpl = vi
      .fn<FetchLike>()
      .mockRejectedValueOnce(new Error("connect reset"))
      .mockResolvedValueOnce({ status: 204 });
    const sleep = vi.fn(async () => {});
    const logs: string[] = [];
    const result = await dispatchPoll(TOKEN, fetchImpl, sleep, (m) => logs.push(m));
    expect(result).toEqual({ status: 204, attempts: 2 });
    expect(sleep).toHaveBeenCalledExactlyOnceWith(5000);
    expect(logs).toEqual([
      "dispatch network_error attempt=1",
      "dispatch status=204 attempt=2",
    ]);
  });

  it("does not retry on 401", async () => {
    const fetchImpl = vi.fn<FetchLike>(async () => ({ status: 401 }));
    const sleep = vi.fn(async () => {});
    const logs: string[] = [];
    const result = await dispatchPoll(TOKEN, fetchImpl, sleep, (m) => logs.push(m));
    expect(result).toEqual({ status: 401, attempts: 1 });
    expect(fetchImpl).toHaveBeenCalledOnce();
    expect(sleep).not.toHaveBeenCalled();
  });

  it("does not retry on 404", async () => {
    const fetchImpl = vi.fn<FetchLike>(async () => ({ status: 404 }));
    const sleep = vi.fn(async () => {});
    const result = await dispatchPoll(TOKEN, fetchImpl, sleep, () => {});
    expect(result).toEqual({ status: 404, attempts: 1 });
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
    await dispatchPoll(TOKEN, fetchImpl, sleep, (m) => logs.push(m));
    const joined = logs.join("\n");
    expect(joined).not.toContain(TOKEN);
    expect(joined.toLowerCase()).not.toContain("authorization");
    expect(joined.toLowerCase()).not.toContain("bearer");
    expect(joined).not.toMatch(/ghs_/);
  });

  it("passes the built request to fetch", async () => {
    const fetchImpl = vi.fn<FetchLike>(async () => ({ status: 204 }));
    await dispatchPoll(TOKEN, fetchImpl, async () => {}, () => {});
    const expected = buildDispatchRequest(TOKEN);
    expect(fetchImpl).toHaveBeenCalledWith(expected.url, {
      method: expected.method,
      headers: expected.headers,
      body: expected.body,
    });
  });
});

describe("fetch handler", () => {
  it("always returns 404", async () => {
    const mod = await import("../src/index");
    const res = await mod.default.fetch();
    expect(res.status).toBe(404);
  });
});
