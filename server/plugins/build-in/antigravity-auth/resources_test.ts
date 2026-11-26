import type { JsonValue, NetworkRequestInit, PluginContext } from "cursor-byok:plugin";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { parseCredentialFiles, refreshAccount, RESOURCE_TYPE } from "./resources.ts";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function context(requests: string[]): PluginContext["network"] {
  return {
    fetch: async (url: string, init: NetworkRequestInit = {}) => {
      requests.push(`${url}:${init.body ?? ""}`);
      return {
        status: 200,
        headers: {},
        body: JSON.stringify({
          access_token: "access-token",
          refresh_token: "refresh-token",
          expires_in: 3600,
        }),
      };
    },
    stream: () => Promise.reject(new Error("stream is not expected")),
  };
}

Deno.test("imports an array of email and refresh_token credentials", async () => {
  const requests: string[] = [];
  const result = await parseCredentialFiles([
    {
      name: "antigravity.json",
      content: JSON.stringify([
        { email: "teocougar@gmail.com", refresh_token: "refresh-token" },
      ]),
    },
  ], context(requests));

  assert(result.warnings.length === 0, "the credential file should not produce warnings");
  assert(result.credentials.length === 1, "one credential should be imported");
  assert(
    result.credentials[0].displayName === "teocougar@gmail.com",
    "email should be used as display name",
  );
  assert(
    result.credentials[0].accessToken === "access-token",
    "refresh token should be exchanged for an access token",
  );
  assert(
    result.credentials[0].refreshToken === "refresh-token",
    "refresh token should be preserved",
  );
  assert(requests.length === 1, "the refresh token should be exchanged once");
});

Deno.test("refresh persists rotated Google tokens and account metadata", async () => {
  const requests: string[] = [];
  const resource: ResourceSnapshot = {
    id: "resource-1",
    type: RESOURCE_TYPE,
    key: "antigravity:user@example.com",
    privateData: {
      accessToken: "old-access-token",
      refreshToken: "old-refresh-token",
      displayName: "user@example.com",
      projectId: "old-project",
      quota: null,
    },
    state: { status: "ready" },
  };
  const network: PluginContext["network"] = {
    fetch: async (url: string) => {
      requests.push(url);
      if (url === "https://oauth2.googleapis.com/token") {
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            access_token: "new-access-token",
            refresh_token: "new-refresh-token",
            expires_in: 3600,
          }),
        };
      }
      if (url.includes("loadCodeAssist")) {
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({ cloudaicompanionProject: "new-project", paidTier: { id: "pro" } }),
        };
      }
      return { status: 200, headers: {}, body: JSON.stringify({ models: {} }) };
    },
    stream: () => Promise.reject(new Error("stream is not expected")),
  };
  const patch = await refreshAccount(resource, { network, signal: new AbortController().signal });
  const data = patch.privateData as Record<string, JsonValue>;

  assert(patch.state?.status === "ready", "a valid refreshed account should remain ready");
  assert(data.accessToken === "new-access-token", "access token rotation should be saved");
  assert(data.refreshToken === "new-refresh-token", "refresh token rotation should be saved");
  assert(data.projectId === "new-project", "refreshed project metadata should be saved");
  assert(requests.length === 3, "refresh, project lookup, and model quota lookup should run");
});

Deno.test("marks a revoked Google refresh token invalid", async () => {
  const resource: ResourceSnapshot = {
    id: "resource-1",
    type: RESOURCE_TYPE,
    key: "antigravity:user@example.com",
    privateData: {
      accessToken: "old-access-token",
      refreshToken: "revoked-refresh-token",
      displayName: "user@example.com",
      quota: null,
    },
    state: { status: "ready" },
  };
  const network: PluginContext["network"] = {
    fetch: async () => ({
      status: 400,
      headers: {},
      body: JSON.stringify({ error: "invalid_grant" }),
    }),
    stream: () => Promise.reject(new Error("stream is not expected")),
  };
  const patch = await refreshAccount(resource, { network, signal: new AbortController().signal });

  assert(
    patch.state?.status === "invalid",
    "a revoked refresh token should invalidate the account",
  );
});
