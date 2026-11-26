import type {
  JsonValue,
  NetworkEventStream,
  NetworkResponse,
  PluginContext,
} from "cursor-byok:plugin";
import type { LlmRequest, ModelEvent } from "cursor-byok:provider";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { __descriptor, __getRegisteredPlugin } from "cursor-byok:plugin";
import "./main.ts";
import { codexDeviceOAuth } from "./oauth.ts";
import { parseOfficialModels } from "./models.ts";
import { buildResponsesBody } from "cursor-byok:protocol/openai-responses";
import { codexProvider, isQuotaError } from "./provider.ts";
import {
  accountIdentity,
  consumeResetCardAction,
  credentialDraft,
  listResetCardsAction,
  parseCodexUsage,
  parseCredentialFiles,
  presentAccount,
  quotaState,
  refreshAccount,
  refreshAccountToken,
  RESOURCE_TYPE,
} from "./resources.ts";

function assert(condition: unknown, message = "assertion failed"): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

function jwt(payload: Record<string, unknown>): string {
  const encoded = btoa(JSON.stringify(payload)).replace(/=/g, "").replace(/\+/g, "-").replace(
    /\//g,
    "_",
  );
  return `header.${encoded}.signature`;
}

type RequestInit = { body?: string; headers?: Record<string, string> };
type FetchHandler = (url: string, init?: RequestInit) => NetworkResponse;
type StreamHandler = (url: string, init?: RequestInit) => NetworkEventStream;

function context(handlers: { fetch?: FetchHandler; stream?: StreamHandler }): PluginContext {
  return {
    network: {
      fetch: (url: string, init?: RequestInit) => {
        if (!handlers.fetch) throw new Error("fetch was not expected");
        return Promise.resolve(handlers.fetch(url, init));
      },
      stream: (url: string, init?: RequestInit) => {
        if (!handlers.stream) throw new Error("stream was not expected");
        return Promise.resolve(handlers.stream(url, init));
      },
    },
    signal: new AbortController().signal,
  };
}

function snapshot(privateData: JsonValue): ResourceSnapshot {
  return {
    id: "resource-1",
    type: RESOURCE_TYPE,
    key: "codex:acct-1",
    privateData,
    state: { status: "ready" },
  };
}

async function* sse(lines: string[]): AsyncGenerator<string> {
  for (const line of lines) yield line;
}

Deno.test("plugin descriptor requests resource refresh before model listing", () => {
  const descriptor = __descriptor(__getRegisteredPlugin());
  assertEquals(descriptor.providers[0].refreshResourceBeforeList, true);
});

function request(): LlmRequest {
  return {
    instructions: "You are a coding assistant.",
    messages: [{ role: "user", content: [{ type: "text", text: "hi" }] }],
    tools: [],
    reasoning: { enabled: true, effort: "medium" },
    latency: "fast",
    maxOutputTokens: 128_000,
    cacheKey: "conversation-1",
  };
}

Deno.test("account identity prioritizes ChatGPT account ID and drafts keep tokens private-side", async () => {
  const token = jwt({
    "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" },
    sub: "subject-1",
    email: "person@example.com",
  });
  assertEquals(await accountIdentity(token), {
    key: "codex:acct-1",
    displayName: "person@example.com",
  });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  assertEquals(draft.key, "codex:acct-1");
  const view = presentAccount(snapshot(draft.privateData));
  assert(!JSON.stringify(view).includes(token), "resource view exposed an access token");
  assertEquals(view.displayName, "person@example.com");
});

Deno.test("credential import accepts Codex auth JSON files", () => {
  const { credentials, warnings } = parseCredentialFiles([
    {
      name: "auth.json",
      content: JSON.stringify({
        tokens: {
          access_token: "access-secret",
          refresh_token: "refresh-secret",
          id_token: jwt({ email: "person@example.com" }),
        },
      }),
    },
    { name: "broken.json", content: "{not json" },
  ]);
  assertEquals(credentials, [{
    accessToken: "access-secret",
    refreshToken: "refresh-secret",
    displayName: "person@example.com",
  }]);
  assertEquals(warnings, ["broken.json: not valid JSON"]);
});

Deno.test("refresh token rotation is returned for persistence", async () => {
  const resource = snapshot({
    accessToken: "old-access-token",
    refreshToken: "old-refresh-token",
    accountId: "acct-1",
    displayName: "person@example.com",
    quota: null,
  });
  let requestBody = "";
  const patch = await refreshAccountToken(
    resource,
    context({
      fetch: (url, init) => {
        assertEquals(url, "https://auth.openai.com/oauth/token");
        requestBody = init?.body ?? "";
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            access_token: "new-access-token",
            refresh_token: "rotated-refresh-token",
          }),
        };
      },
    }),
  );
  const data = patch.privateData as Record<string, unknown>;
  const form = new URLSearchParams(requestBody);

  assertEquals(form.get("grant_type"), "refresh_token");
  assertEquals(form.get("refresh_token"), "old-refresh-token");
  assertEquals(data.accessToken, "new-access-token");
  assertEquals(data.refreshToken, "rotated-refresh-token");
  assertEquals(patch.state, { status: "ready" });
});

Deno.test("invalid_grant invalidates a revoked Codex refresh token", async () => {
  const resource = snapshot({
    accessToken: "old-access-token",
    refreshToken: "revoked-refresh-token",
    accountId: "acct-1",
    displayName: "person@example.com",
    quota: null,
  });
  const patch = await refreshAccountToken(
    resource,
    context({
      fetch: () => ({
        status: 400,
        headers: {},
        body: JSON.stringify({ error: "invalid_grant" }),
      }),
    }),
  );

  assertEquals(patch.state, {
    status: "invalid",
    message: "ChatGPT authorization expired; sign in again",
  });
});

Deno.test("temporary Codex refresh failures do not invalidate the account", async () => {
  const resource = snapshot({
    accessToken: "old-access-token",
    refreshToken: "refresh-token",
    accountId: "acct-1",
    displayName: "person@example.com",
    quota: null,
  });
  let requestCount = 0;
  const patch = await refreshAccount(
    resource,
    context({
      fetch: () => {
        requestCount += 1;
        return { status: 503, headers: {}, body: "service unavailable" };
      },
    }),
  );

  assert(requestCount === 1, "usage lookup should wait until OAuth refresh succeeds");
  assertEquals(patch.state, undefined);
});

Deno.test("usage maps secondary to weekly and primary to five-hour quota", () => {
  const quota = parseCodexUsage({
    plan_type: "plus",
    rate_limit: {
      primary_window: { used_percent: 80, reset_at: 1_800_000_000 },
      secondary_window: { used_percent: 25, reset_at: 1_900_000_000 },
    },
    rate_limit_reset_credits: { available_count: 2 },
  }, 1_700_000_000_000);
  assertEquals(quota.planLabel, "ChatGPT Plus");
  assertEquals(quota.weekly?.remainingPercent, 75);
  assertEquals(quota.fiveHour?.remainingPercent, 20);
  assertEquals(quota.weekly?.resetAtMs, 1_900_000_000_000);
  assertEquals(quota.resetCreditsAvailable, 2);
  assertEquals(quotaState(quota, 1_700_000_000_000), { status: "ready" });
});

Deno.test("reset card action lists safe card metadata and optional expiry", async () => {
  const result = await listResetCardsAction.run(
    snapshot({
      accessToken: "access-secret",
      accountId: "acct-1",
      refreshToken: null,
      displayName: "person@example.com",
      quota: null,
    }),
    null,
    context({
      fetch: (url, init) => {
        assertEquals(url, "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits");
        assertEquals(init?.headers?.["ChatGPT-Account-Id"], "acct-1");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            available_count: 1,
            credits: [{
              id: "credit-1",
              reset_type: "codex_rate_limits",
              status: "available",
              granted_at: "2026-06-12T01:33:14Z",
              expires_at: "2026-07-12T01:33:14Z",
              title: "One free rate limit reset",
            }],
          }),
        };
      },
    }),
  );
  assertEquals(result.cards, [{
    id: "credit-1",
    title: "One free rate limit reset",
    status: "available",
    grantedAtMs: Date.parse("2026-06-12T01:33:14Z"),
    expiresAtMs: Date.parse("2026-07-12T01:33:14Z"),
    fields: [{
      id: "reset-type",
      label: { "en-US": "Reset type", "zh-CN": "重置类型" },
      value: "codex_rate_limits",
    }],
  }]);
});

Deno.test("reset card action consumes a selected card and refreshes quota state", async () => {
  let requestNumber = 0;
  const result = await consumeResetCardAction.run(
    snapshot({
      accessToken: "access-secret",
      accountId: "acct-1",
      refreshToken: null,
      displayName: "person@example.com",
      quota: null,
    }),
    { cardId: "credit-1" },
    context({
      fetch: (url, init) => {
        requestNumber += 1;
        if (requestNumber === 1 || requestNumber === 4) {
          assertEquals(url, "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits");
          return {
            status: 200,
            headers: {},
            body: JSON.stringify(
              requestNumber === 1
                ? { available_count: 1, credits: [{ id: "credit-1", status: "available" }] }
                : { available_count: 0, credits: [] },
            ),
          };
        }
        if (requestNumber === 2) {
          assertEquals(
            url,
            "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume",
          );
          assertEquals(JSON.parse(init?.body ?? "{}"), {
            credit_id: "credit-1",
            redeem_request_id: JSON.parse(init?.body ?? "{}").redeem_request_id,
          });
          return { status: 200, headers: {}, body: JSON.stringify({ code: "reset" }) };
        }
        assertEquals(url, "https://chatgpt.com/backend-api/wham/usage");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({ rate_limit_reset_credits: { available_count: 0 } }),
        };
      },
    }),
  );
  assertEquals(requestNumber, 4);
  assertEquals(result.cards, []);
  const quota = (result.patch?.privateData as Record<string, unknown>).quota as Record<
    string,
    unknown
  >;
  assertEquals(quota.resetCreditsAvailable, 0);
  assertEquals(quota.weekly, null);
  assertEquals(quota.fiveHour, null);
});

Deno.test("exhausted quota projects a cooling state until the latest reset", () => {
  const quota = parseCodexUsage({
    rate_limit: {
      primary_window: { used_percent: 100, reset_at: 1_800_000_000 },
      secondary_window: { used_percent: 100, reset_at: 1_900_000_000 },
    },
  }, 1_700_000_000_000);
  assertEquals(quotaState(quota, 1_700_000_000_000), {
    status: "cooling",
    retryAtMs: 1_900_000_000_000,
    message: "ChatGPT quota is exhausted",
  });
});

Deno.test("official model discovery excludes hidden models and puts the default first", () => {
  const models = parseOfficialModels({
    default_model: "gpt-second",
    models: [
      {
        slug: "gpt-first",
        display_name: "GPT First",
        supported_in_api: true,
        visibility: "list",
        supported_reasoning_levels: [
          { effort: "low", description: "Fast responses" },
          { effort: "medium", description: "Balanced" },
        ],
      },
      { slug: "gpt-second", supported_in_api: true, visibility: "list" },
      { slug: "gpt-hidden", supported_in_api: true, visibility: "hidden" },
      { slug: "gpt-internal", supported_in_api: false, visibility: "list" },
    ],
  });
  assertEquals(models.map((model) => model.id), ["gpt-second", "gpt-first"]);
  assertEquals(models[1].capabilities, { images: true });
  assertEquals(models[1].privateData, { reasoningEfforts: ["low", "medium"] });
});

Deno.test("device OAuth begins with a host-held session and completes with a resource draft", async () => {
  const accessToken = jwt({
    "https://api.openai.com/auth": { chatgpt_account_id: "acct-oauth" },
    email: "oauth@example.com",
  });
  let requestNumber = 0;
  const flowContext = context({
    fetch: (url, init) => {
      requestNumber += 1;
      if (requestNumber === 1) {
        assertEquals(url, "https://auth.openai.com/api/accounts/deviceauth/usercode");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            device_auth_id: "private-device-id",
            user_code: "ABCD-EFGH",
            expires_in: 900,
            interval: 5,
          }),
        };
      }
      if (requestNumber === 2) {
        assertEquals(url, "https://auth.openai.com/api/accounts/deviceauth/token");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            authorization_code: "authorization-code",
            code_verifier: "pkce-verifier",
          }),
        };
      }
      assertEquals(url, "https://auth.openai.com/oauth/token");
      assert(init?.body?.includes("grant_type=authorization_code"));
      assert(init?.body?.includes("code_verifier=pkce-verifier"));
      return {
        status: 200,
        headers: {},
        body: JSON.stringify({ access_token: accessToken, refresh_token: "refresh-secret" }),
      };
    },
  });

  const begun = await codexDeviceOAuth.begin(flowContext);
  assertEquals(begun.userCode, "ABCD-EFGH");
  assertEquals(begun.pollIntervalMs, 5000);

  const polled = await codexDeviceOAuth.poll(begun.session, flowContext);
  assert(polled.status === "completed", `expected completed, received ${polled.status}`);
  assertEquals(polled.resources[0].key, "codex:acct-oauth");
  assertEquals(requestNumber, 3);
});

Deno.test("invoke streams normalized events from the Codex Responses API", async () => {
  const token = jwt({ "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" } });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  let requestBody = "";
  let requestHeaders: Record<string, string> = {};
  const events: ModelEvent[] = [];
  const result = await codexProvider.invoke(
    {
      model: {
        id: "gpt-test",
        displayName: "GPT Test",
        privateData: { reasoningEfforts: ["medium"] },
      },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: (event: ModelEvent) => events.push(event) },
    context({
      stream: (url, init) => {
        assertEquals(url, "https://chatgpt.com/backend-api/codex/responses");
        requestBody = init?.body ?? "";
        requestHeaders = init?.headers ?? {};
        return {
          status: 200,
          headers: {},
          lines: sse([
            'data: {"type":"response.output_text.delta","delta":"Hel"}',
            'data: {"type":"response.output_text.delta","delta":"lo"}',
            'data: {"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":2,"input_tokens_details":{"cached_tokens":4}}}}',
          ]),
        };
      },
    }),
  );
  assertEquals(result, { status: "completed" });
  const body = JSON.parse(requestBody) as Record<string, unknown>;
  assertEquals(body.model, "gpt-test");
  assertEquals(body.store, false);
  assertEquals(body.reasoning, { summary: "auto", effort: "medium" });
  assertEquals(body.instructions, "You are a coding assistant.");
  assertEquals(body.include, ["reasoning.encrypted_content"]);
  assert(!("max_output_tokens" in body), "Codex endpoint rejects max_output_tokens");
  assertEquals(body.service_tier, "priority");
  assertEquals(body.prompt_cache_key, "conversation-1");
  // 缓存亲和头与 prompt_cache_key 同源。
  assertEquals(requestHeaders["session-id"], "conversation-1");
  assertEquals(requestHeaders["thread-id"], "conversation-1");
  assertEquals(requestHeaders["x-client-request-id"], "conversation-1");
  assertEquals(events, [
    { type: "text-start" },
    { type: "text-delta", text: "Hel" },
    { type: "text-delta", text: "lo" },
    {
      type: "usage",
      usage: {
        inputTokens: 10,
        outputTokens: 2,
        totalTokens: null,
        cacheReadTokens: 4,
        cacheWriteTokens: null,
        reasoningTokens: null,
      },
    },
    { type: "text-end" },
    { type: "done", reason: "stop" },
  ]);
});

Deno.test("invoke refreshes an expired Codex token and retries once", async () => {
  const resource = snapshot({
    accessToken: "old-access-token",
    refreshToken: "old-refresh-token",
    accountId: "acct-1",
    displayName: "person@example.com",
    quota: null,
  });
  const authorizationHeaders: string[] = [];
  let streamCount = 0;
  const result = await codexProvider.invoke(
    {
      model: { id: "gpt-test", displayName: "GPT Test", privateData: { reasoningEfforts: [] } },
      resource,
      request: request(),
    },
    { emit: () => undefined },
    context({
      fetch: (url) => {
        assertEquals(url, "https://auth.openai.com/oauth/token");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            access_token: "new-access-token",
            refresh_token: "new-refresh-token",
          }),
        };
      },
      stream: (_url, init) => {
        streamCount += 1;
        authorizationHeaders.push(init?.headers?.authorization ?? "");
        if (streamCount === 1) {
          return { status: 401, headers: {}, lines: sse(["data: expired"]) };
        }
        return {
          status: 200,
          headers: {},
          lines: sse(['data: {"type":"response.completed","response":{}}']),
        };
      },
    }),
  );

  assertEquals(result.status, "completed");
  assertEquals(streamCount, 2);
  assertEquals(authorizationHeaders, ["Bearer old-access-token", "Bearer new-access-token"]);
  assertEquals(
    (result.patch?.privateData as Record<string, unknown>).refreshToken,
    "new-refresh-token",
  );
});

Deno.test("reasoning replay projects response items to valid input items", () => {
  const replayRequest = request();
  replayRequest.messages = [{
    role: "assistant",
    text: "",
    thinking: "",
    replayState: {
      providerKind: "openai_responses",
      value: {
        items: [{
          type: "reasoning",
          id: "item-1",
          status: "completed",
          summary: [{ type: "summary_text", text: "why" }],
          content: [],
          encrypted_content: "opaque",
          output_only: true,
        }],
      },
    },
    toolCalls: [],
  }];

  const body = buildResponsesBody({
    url: "https://example.com/responses",
    model: "gpt-test",
    request: replayRequest,
  });
  assertEquals(body.input, [{
    type: "reasoning",
    id: "item-1",
    summary: [{ type: "summary_text", text: "why" }],
    content: [],
    encrypted_content: "opaque",
  }]);
});

Deno.test("invoke streams incremental tool calls and replays reasoning items", async () => {
  const token = jwt({ "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" } });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const events: ModelEvent[] = [];
  const result = await codexProvider.invoke(
    {
      model: { id: "gpt-test", displayName: "GPT Test" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: (event: ModelEvent) => events.push(event) },
    context({
      stream: () => ({
        status: 200,
        headers: {},
        lines: sse([
          'data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"read_file"}}',
          'data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\\"path\\":"}',
          'data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"\\"a.ts\\"}"}',
          'data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{\\"path\\":\\"a.ts\\"}"}}',
          'data: {"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","encrypted_content":"opaque"}}',
          'data: {"type":"response.completed","response":{}}',
        ]),
      }),
    }),
  );
  assertEquals(result, { status: "completed" });
  assertEquals(events, [
    { type: "tool-call-start", index: 0, callId: "call-1", name: "read_file" },
    { type: "tool-call-arguments-delta", index: 0, delta: '{"path":' },
    { type: "tool-call-arguments-delta", index: 0, delta: '"a.ts"}' },
    { type: "tool-call-end", index: 0 },
    {
      type: "replay-state",
      providerKind: "openai_responses",
      value: { items: [{ type: "reasoning", encrypted_content: "opaque" }] },
    },
    { type: "done", reason: "tool-use" },
  ]);
});

Deno.test("invoke maps quota failures to a cooling resource error", async () => {
  assert(!isQuotaError("429 rate_limit_reached"));
  assert(isQuotaError("429 usage_limit_reached: 5-hour limit"));
  assert(isQuotaError("credit_balance_exhausted"));
  const token = jwt({ "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" } });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const result = await codexProvider.invoke(
    {
      model: { id: "gpt-test", displayName: "GPT Test" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: () => ({
        status: 429,
        headers: {},
        lines: sse(['{"detail":"usage_limit_reached","reset_after_seconds":600}']),
      }),
    }),
  );
  assert(result.status === "resource-error", `expected resource-error, received ${result.status}`);
  assert(result.patch.state?.status === "cooling", "quota failure should cool the resource");
  assert(
    result.patch.state.retryAtMs !== undefined && result.patch.state.retryAtMs > Date.now(),
    "cooling should carry the parsed reset time",
  );
});

Deno.test("invoke surfaces both OpenAI Responses error event forms", async () => {
  const token = jwt({ "https://api.openai.com/auth": { chatgpt_account_id: "acct-1" } });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  for (
    const event of [
      'data: {"type":"error","error":{"code":"api_key_spend_limit_exceeded","message":"API spend limit reached"}}',
      'data: {"type":"response.failed","response":{"error":{"code":"project_spend_limit_exceeded","message":"Project spend limit reached"}}}',
    ]
  ) {
    const result = await codexProvider.invoke(
      {
        model: { id: "gpt-test", displayName: "GPT Test" },
        resource: snapshot(draft.privateData),
        request: request(),
      },
      { emit: () => {} },
      context({
        stream: () => ({ status: 200, headers: {}, lines: sse([event]) }),
      }),
    );
    assert(result.status === "request-error", `expected request-error, received ${result.status}`);
    assert(result.message.includes("spend_limit_exceeded"), "error code should be preserved");
    assert(result.message.includes("spend limit reached"), "provider detail should be preserved");
  }
});
