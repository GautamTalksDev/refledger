/**
 * Refledger poll clock: cron-triggered workflow_dispatch. No public HTTP surface.
 */

/** Contact URL promised by OPERATIONS.md §2. */
export const OPERATIONS_CONTACT =
  "https://raw.githubusercontent.com/GautamTalksDev/refledger/main/OPERATIONS.md";

export const USER_AGENT = `refledger-clock (+${OPERATIONS_CONTACT})`;

export const DISPATCH_URL =
  "https://api.github.com/repos/GautamTalksDev/refledger/actions/workflows/poll.yml/dispatches";

export const DISPATCH_BODY = JSON.stringify({ ref: "main" });

export interface Env {
  DISPATCH_TOKEN: string;
}

export interface DispatchRequest {
  url: string;
  method: "POST";
  headers: Record<string, string>;
  body: string;
}

/** Build the GitHub workflow_dispatch request. Token is only placed in Authorization. */
export function buildDispatchRequest(token: string): DispatchRequest {
  return {
    url: DISPATCH_URL,
    method: "POST",
    headers: {
      Authorization: `Bearer ${token}`,
      Accept: "application/vnd.github+json",
      "X-GitHub-Api-Version": "2022-11-28",
      "User-Agent": USER_AGENT,
      "Content-Type": "application/json",
    },
    body: DISPATCH_BODY,
  };
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
 * Logs status (and attempt) only; never logs the token or headers.
 */
export async function dispatchPoll(
  token: string,
  fetchImpl: FetchLike,
  sleep: SleepFn,
  log: LogFn,
): Promise<{ status: number; attempts: number }> {
  const req = buildDispatchRequest(token);
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
      log(`dispatch status=${res.status} attempt=${attempts}`);
      if (res.status === 204) {
        return { status: res.status, attempts };
      }
      if (shouldRetry(res.status) && attempts < 2) {
        await sleep(5000);
        continue;
      }
      return { status: res.status, attempts };
    } catch {
      log(`dispatch network_error attempt=${attempts}`);
      if (attempts < 2) {
        await sleep(5000);
        continue;
      }
      return { status: lastStatus, attempts };
    }
  }

  return { status: lastStatus, attempts };
}

const worker = {
  async scheduled(
    _controller: ScheduledController,
    env: Env,
    _ctx: ExecutionContext,
  ): Promise<void> {
    await dispatchPoll(
      env.DISPATCH_TOKEN,
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
