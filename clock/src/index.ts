/**
 * Refledger clock: cron-triggered workflow_dispatch for poll and canary.
 * No public HTTP surface.
 */

/** Contact URL promised by OPERATIONS.md §2. */
export const OPERATIONS_CONTACT =
  "https://raw.githubusercontent.com/GautamTalksDev/refledger/main/OPERATIONS.md";

export const USER_AGENT = `refledger-clock (+${OPERATIONS_CONTACT})`;

/** Poll cadence: every 5 minutes at :02, :07, … :57 (never :00). */
export const POLL_CRON = "2-57/5 * * * *";

/** Canary rotation: minute 17 of every 4th hour. */
export const CANARY_CRON = "17 */4 * * *";

export const POLL_DISPATCH_URL =
  "https://api.github.com/repos/GautamTalksDev/refledger/actions/workflows/poll.yml/dispatches";

export const CANARY_DISPATCH_URL =
  "https://api.github.com/repos/GautamTalksDev/canary/actions/workflows/canary.yml/dispatches";

export const DISPATCH_BODY = JSON.stringify({ ref: "main" });

/** @deprecated Use POLL_DISPATCH_URL. Kept for existing imports in docs/tests. */
export const DISPATCH_URL = POLL_DISPATCH_URL;

export type DispatchTarget = "poll" | "canary";

export interface Env {
  DISPATCH_TOKEN: string;
}

export interface DispatchRequest {
  url: string;
  method: "POST";
  headers: Record<string, string>;
  body: string;
  /** Short name for logs only (never a secret). */
  target: DispatchTarget;
}

/** Build a GitHub workflow_dispatch request. Token is only placed in Authorization. */
export function buildDispatchRequest(
  token: string,
  target: DispatchTarget = "poll",
): DispatchRequest {
  const url = target === "canary" ? CANARY_DISPATCH_URL : POLL_DISPATCH_URL;
  return {
    url,
    method: "POST",
    headers: {
      Authorization: `Bearer ${token}`,
      Accept: "application/vnd.github+json",
      "X-GitHub-Api-Version": "2022-11-28",
      "User-Agent": USER_AGENT,
      "Content-Type": "application/json",
    },
    body: DISPATCH_BODY,
    target,
  };
}

/** Map a Cloudflare cron expression to the workflow it should dispatch. */
export function targetForCron(cron: string): DispatchTarget | null {
  if (cron === POLL_CRON) {
    return "poll";
  }
  if (cron === CANARY_CRON) {
    return "canary";
  }
  return null;
}

export type FetchLike = (
  input: string,
  init?: { method?: string; headers?: Record<string, string>; body?: string },
) => Promise<{ status: number }>;

export type SleepFn = (ms: number) => Promise<void>;
export type LogFn = (message: string) => void;

function shouldRetry(status: number): boolean {
  return status >= 500 && status <= 599;
}

/**
 * POST workflow_dispatch. Expects 204.
 * Retries once after 5 seconds on 5xx or network error. Never retries 4xx.
 * Logs status, target, and attempt only; never logs the token or headers.
 */
export async function dispatchWorkflow(
  token: string,
  target: DispatchTarget,
  fetchImpl: FetchLike,
  sleep: SleepFn,
  log: LogFn,
): Promise<{ status: number; attempts: number; target: DispatchTarget }> {
  const req = buildDispatchRequest(token, target);
  let attempts = 0;
  let lastStatus = 0;

  while (attempts < 2) {
    attempts += 1;
    try {
      const res = await fetchImpl(req.url, {
        method: req.method,
        headers: req.headers,
        body: req.body,
      });
      lastStatus = res.status;
      log(`dispatch target=${target} status=${res.status} attempt=${attempts}`);
      if (res.status === 204) {
        return { status: res.status, attempts, target };
      }
      if (shouldRetry(res.status) && attempts < 2) {
        await sleep(5000);
        continue;
      }
      return { status: res.status, attempts, target };
    } catch {
      log(`dispatch target=${target} network_error attempt=${attempts}`);
      if (attempts < 2) {
        await sleep(5000);
        continue;
      }
      return { status: lastStatus, attempts, target };
    }
  }

  return { status: lastStatus, attempts, target };
}

/** @deprecated Prefer dispatchWorkflow(token, "poll", …). */
export async function dispatchPoll(
  token: string,
  fetchImpl: FetchLike,
  sleep: SleepFn,
  log: LogFn,
): Promise<{ status: number; attempts: number }> {
  const r = await dispatchWorkflow(token, "poll", fetchImpl, sleep, log);
  return { status: r.status, attempts: r.attempts };
}

const worker = {
  async scheduled(
    controller: ScheduledController,
    env: Env,
    _ctx: ExecutionContext,
  ): Promise<void> {
    const target = targetForCron(controller.cron);
    if (target === null) {
      console.log(`dispatch unknown_cron=${controller.cron}`);
      return;
    }
    await dispatchWorkflow(
      env.DISPATCH_TOKEN,
      target,
      globalThis.fetch as FetchLike,
      (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
      (msg) => console.log(msg),
    );
  },

  async fetch(): Promise<Response> {
    return new Response("Not Found", { status: 404 });
  },
};

export default worker;
