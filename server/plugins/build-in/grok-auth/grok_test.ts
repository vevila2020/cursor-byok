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
import { grokDeviceOAuth } from "./oauth.ts";
import { FALLBACK_MODELS, grokModels, parseGrokModels } from "./models.ts";
import { grokProvider, isQuotaError } from "./provider.ts";
import {
  accountIdentity,
  credentialDraft,
  parseCredentialFiles,
  parseGrokUsage,
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
    key: "grok:user-1",
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
    maxOutputTokens: 32_000,
    cacheKey: "conversation-1",
  };
}

Deno.test("account identity uses the JWT subject and drafts keep tokens private-side", async () => {
  const token = jwt({ sub: "user-1", email: "person@x.ai" });
  assertEquals(await accountIdentity(token), {
    key: "grok:user-1",
    displayName: "person@x.ai",
  });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  assertEquals(draft.key, "grok:user-1");
  const view = presentAccount(snapshot(draft.privateData));
  assert(!JSON.stringify(view).includes(token), "resource view exposed an access token");
  assertEquals(view.displayName, "person@x.ai");
});

Deno.test("credential import accepts Grok credential JSON files", () => {
  const { credentials, warnings } = parseCredentialFiles([
    {
      name: "accounts.json",
      content: JSON.stringify({
        accounts: [
          { access_token: "token-1", refresh_token: "refresh-1", email: "a@x.ai" },
          { access_token: "token-2", disabled: true },
        ],
      }),
    },
    { name: "broken.json", content: "{not json" },
  ]);
  assertEquals(credentials, [{
    accessToken: "token-1",
    refreshToken: "refresh-1",
    displayName: "a@x.ai",
  }]);
  assertEquals(warnings, ["broken.json: not valid JSON"]);
});

Deno.test("refresh token rotation is returned for persistence", async () => {
  const resource = snapshot({
    accessToken: "old-access-token",
    refreshToken: "old-refresh-token",
    displayName: "person@x.ai",
    quota: null,
  });
  let requestBody = "";
  const patch = await refreshAccountToken(
    resource,
    context({
      fetch: (url, init) => {
        assertEquals(url, "https://auth.x.ai/oauth2/token");
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

Deno.test("invalid_grant invalidates a revoked Grok refresh token", async () => {
  const resource = snapshot({
    accessToken: "old-access-token",
    refreshToken: "revoked-refresh-token",
    displayName: "person@x.ai",
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
    message: "Grok authorization expired; sign in again",
  });
});

Deno.test("temporary Grok refresh failures do not invalidate the account", async () => {
  const resource = snapshot({
    accessToken: "old-access-token",
    refreshToken: "refresh-token",
    displayName: "person@x.ai",
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

Deno.test("credit usage percent is inverted to remaining and drives cooling", () => {
  const quota = parseGrokUsage({
    config: {
      creditUsagePercent: 34,
      subscriptionTierDisplay: "SuperGrok",
      currentPeriod: { end: "2026-09-01T00:00:00Z" },
    },
  }, 1_700_000_000_000);
  assertEquals(quota.planLabel, "SuperGrok");
  assertEquals(quota.remainingPercent, 66);
  assertEquals(quota.resetAtMs, Date.parse("2026-09-01T00:00:00Z"));
  assertEquals(quotaState(quota, 1_700_000_000_000), { status: "ready" });

  const exhausted = parseGrokUsage({
    config: { creditUsagePercent: 100, currentPeriod: { end: 1_900_000_000 } },
  }, 1_700_000_000_000);
  assertEquals(quotaState(exhausted, 1_700_000_000_000), {
    status: "cooling",
    retryAtMs: 1_900_000_000_000,
    message: "Grok credits are exhausted",
  });
});

Deno.test("missing usage with a billing period counts as unused", () => {
  const quota = parseGrokUsage({ config: { currentPeriod: { end: 1_900_000_000 } } });
  assertEquals(quota.remainingPercent, 100);
  assertEquals(quota.limitReached, false);
});

Deno.test("model discovery parses both language-models and standard list shapes", () => {
  const richModels = parseGrokModels({
    models: [
      { id: "grok-4", input_modalities: ["text", "image"], context_window: 256_000 },
      { id: "grok-3-mini", input_modalities: ["text"] },
      { id: "grok-4" },
    ],
  });
  assertEquals(richModels.map((model) => model.id), ["grok-4", "grok-3-mini"]);
  assertEquals(richModels[0].displayName, "Grok 4");
  assertEquals(richModels[0].capabilities, { images: true });
  assertEquals(richModels[1].capabilities, { images: false });

  const plainModels = parseGrokModels({ data: [{ id: "grok-4-fast" }] });
  assertEquals(plainModels.map((model) => model.id), ["grok-4-fast"]);
  assertEquals(plainModels[0].displayName, "Grok 4 Fast");
});

Deno.test("model discovery falls back to known models when the account cannot list", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const models = await grokModels.list(
    { resource: snapshot(draft.privateData) },
    context({
      fetch: () => ({
        status: 403,
        headers: {},
        body: JSON.stringify({ code: "personal-team-blocked:spending-limit" }),
      }),
    }),
  );
  assertEquals(models, FALLBACK_MODELS);
});

Deno.test("model discovery does not mask an unauthorized account as a fallback catalog", async () => {
  const resource = snapshot({
    accessToken: "expired-access-token",
    refreshToken: null,
    displayName: "person@x.ai",
    quota: null,
  });
  let failed = false;
  let requestHeaders: Record<string, string> = {};
  try {
    await grokModels.list(
      { resource },
      context({
        fetch: (_url, init) => {
          requestHeaders = init?.headers ?? {};
          return { status: 401, headers: {}, body: "unauthorized" };
        },
      }),
    );
  } catch {
    failed = true;
  }

  assert(failed, "an unauthorized response should preserve the existing model catalog");
  assertEquals(requestHeaders["x-xai-token-auth"], "xai-grok-cli");
});

Deno.test("device OAuth begins with a host-held session and completes with a resource draft", async () => {
  const accessToken = jwt({ sub: "user-oauth", email: "oauth@x.ai" });
  let requestNumber = 0;
  const flowContext = context({
    fetch: (url, init) => {
      requestNumber += 1;
      if (requestNumber === 1) {
        assertEquals(url, "https://auth.x.ai/oauth2/device/code");
        assert(init?.body?.includes("scope="), "device code request must carry the scope");
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            device_code: "private-device-code",
            user_code: "ABCD-EFGH",
            verification_uri: "https://accounts.x.ai/activate",
            verification_uri_complete: "https://accounts.x.ai/activate?code=ABCD-EFGH",
            expires_in: 900,
            interval: 5,
          }),
        };
      }
      assertEquals(url, "https://auth.x.ai/oauth2/token");
      assert(init?.body?.includes("device_code=private-device-code"));
      if (requestNumber === 2) {
        return {
          status: 400,
          headers: {},
          body: JSON.stringify({ error: "authorization_pending" }),
        };
      }
      return {
        status: 200,
        headers: {},
        body: JSON.stringify({ access_token: accessToken, refresh_token: "refresh-secret" }),
      };
    },
  });

  const begun = await grokDeviceOAuth.begin(flowContext);
  assertEquals(begun.userCode, "ABCD-EFGH");
  assertEquals(begun.pollIntervalMs, 5000);

  const pending = await grokDeviceOAuth.poll(begun.session, flowContext);
  assertEquals(pending.status, "pending");

  const polled = await grokDeviceOAuth.poll(begun.session, flowContext);
  assert(polled.status === "completed", `expected completed, received ${polled.status}`);
  assertEquals(polled.resources[0].key, "grok:user-oauth");
  assertEquals(requestNumber, 3);
});

Deno.test("invoke streams normalized events from the xAI Chat Completions API", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  let requestBody = "";
  let requestHeaders: Record<string, string> = {};
  const events: ModelEvent[] = [];
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: (event: ModelEvent) => events.push(event) },
    context({
      stream: (url, init) => {
        assertEquals(url, "https://api.x.ai/v1/chat/completions");
        requestBody = init?.body ?? "";
        requestHeaders = init?.headers ?? {};
        return {
          status: 200,
          headers: {},
          lines: sse([
            'data: {"choices":[{"delta":{"content":"Hel"}}]}',
            'data: {"choices":[{"delta":{"content":"lo"}}]}',
            'data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":4}}}',
            "data: [DONE]",
          ]),
        };
      },
    }),
  );
  assertEquals(result, { status: "completed" });
  const body = JSON.parse(requestBody) as Record<string, unknown>;
  assertEquals(body.model, "grok-4");
  assertEquals(body.stream, true);
  assertEquals(body.prompt_cache_key, "conversation-1");
  assert(!("reasoning_effort" in body), "xAI endpoint rejects reasoning_effort");
  assert(!("service_tier" in body), "xAI endpoint rejects service_tier");
  assertEquals(requestHeaders["authorization"], `Bearer ${token}`);
  assertEquals(requestHeaders["x-xai-token-auth"], "xai-grok-cli");
  assertEquals(requestHeaders.accept, "text/event-stream");
  assertEquals(events, [
    { type: "text-start" },
    { type: "text-delta", text: "Hel" },
    { type: "text-delta", text: "lo" },
    { type: "text-end" },
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
    { type: "done", reason: "stop" },
  ]);
});

Deno.test("invoke refreshes an expired Grok token and retries once", async () => {
  const resource = snapshot({
    accessToken: "old-access-token",
    refreshToken: "old-refresh-token",
    displayName: "person@x.ai",
    quota: null,
  });
  const authorizationHeaders: string[] = [];
  const tokenAuthHeaders: string[] = [];
  let streamCount = 0;
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-test", displayName: "Grok Test" },
      resource,
      request: request(),
    },
    { emit: () => undefined },
    context({
      fetch: (url) => {
        assertEquals(url, "https://auth.x.ai/oauth2/token");
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
        tokenAuthHeaders.push(init?.headers?.["x-xai-token-auth"] ?? "");
        if (streamCount === 1) {
          return { status: 401, headers: {}, lines: sse(["data: expired"]) };
        }
        return {
          status: 200,
          headers: {},
          lines: sse(['data: {"choices":[{"delta":{},"finish_reason":"stop"}]}']),
        };
      },
    }),
  );

  assertEquals(result.status, "completed");
  assertEquals(streamCount, 2);
  assertEquals(authorizationHeaders, ["Bearer old-access-token", "Bearer new-access-token"]);
  assertEquals(tokenAuthHeaders, ["xai-grok-cli", "xai-grok-cli"]);
  assertEquals(
    (result.patch?.privateData as Record<string, unknown>).refreshToken,
    "new-refresh-token",
  );
});

Deno.test("invoke streams incremental tool calls and reasoning replay state", async () => {
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const events: ModelEvent[] = [];
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: (event: ModelEvent) => events.push(event) },
    context({
      stream: () => ({
        status: 200,
        headers: {},
        lines: sse([
          'data: {"choices":[{"delta":{"reasoning_content":"thinking"}}]}',
          'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{\\"path\\":"}}]}}]}',
          'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\\"a.ts\\"}"}}]}}]}',
          'data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}',
          "data: [DONE]",
        ]),
      }),
    }),
  );
  assertEquals(result, { status: "completed" });
  assertEquals(events, [
    { type: "thinking-start" },
    { type: "thinking-delta", text: "thinking" },
    { type: "tool-call-start", index: 0, callId: "call-1", name: "read_file" },
    { type: "tool-call-arguments-delta", index: 0, delta: '{"path":' },
    { type: "tool-call-arguments-delta", index: 0, delta: '"a.ts"}' },
    { type: "thinking-end" },
    { type: "tool-call-end", index: 0 },
    {
      type: "replay-state",
      providerKind: "openai_chat",
      value: { reasoning_content: "thinking" },
    },
    { type: "done", reason: "tool-use" },
  ]);
});

Deno.test("invoke maps quota failures to a cooling resource error", async () => {
  assert(!isQuotaError("400 invalid request"));
  assert(isQuotaError("429 credits exhausted"));
  const token = jwt({ sub: "user-1" });
  const draft = await credentialDraft({
    accessToken: token,
    refreshToken: null,
    displayName: null,
  });
  const result = await grokProvider.invoke(
    {
      model: { id: "grok-4", displayName: "Grok 4" },
      resource: snapshot(draft.privateData),
      request: request(),
    },
    { emit: () => {} },
    context({
      stream: () => ({
        status: 429,
        headers: {},
        lines: sse(['{"error":"credits exhausted"}']),
      }),
    }),
  );
  assert(result.status === "resource-error", `expected resource-error, received ${result.status}`);
  assert(result.patch.state?.status === "cooling", "quota failure should cool the resource");
});
