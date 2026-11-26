import type { PluginContext } from "cursor-byok:plugin";
import type { ResourceSnapshot } from "cursor-byok:resource";
import { __descriptor, __getRegisteredPlugin } from "cursor-byok:plugin";
import "./main.ts";
import { antigravityModels, parseAntigravityModels, STATIC_ANTIGRAVITY_MODELS } from "./models.ts";
import { RESOURCE_TYPE } from "./resources.ts";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

Deno.test("plugin descriptor requests resource refresh before model listing", () => {
  const descriptor = __descriptor(__getRegisteredPlugin());
  assert(
    descriptor.providers[0].refreshResourceBeforeList,
    "account refresh should run before listing models",
  );
});

Deno.test("parses only supported dynamic models and filters internal entries", () => {
  const models = parseAntigravityModels({
    models: {
      "gemini-live-model": {
        displayName: "Gemini Live Model",
        supportsThinking: true,
        supportsImages: true,
        maxOutputTokens: 8192,
      },
      chat_20706: { displayName: "Internal model" },
      "invalid-entry": null,
    },
  });

  assert(models.length === 1, "only one supported dynamic model should be returned");
  assert(models[0].id === "gemini-live-model", "the dynamic model ID should be preserved");
  assert(
    models[0].displayName === "Gemini Live Model",
    "the dynamic display name should be preserved",
  );
  assert(models[0].maxOutputTokens === 8192, "the dynamic output limit should be preserved");
  assert(
    JSON.stringify(models[0].privateData) === JSON.stringify({
      reasoningEfforts: ["low", "medium", "high"],
    }),
    "thinking support should map to supported reasoning efforts",
  );
  assert(
    !models.some((model) => model.id === STATIC_ANTIGRAVITY_MODELS[0].id),
    "static catalog entries should not be merged into a successful dynamic response",
  );
});

Deno.test("model discovery returns the live catalog for an authenticated account", async () => {
  const resource: ResourceSnapshot = {
    id: "resource-1",
    type: RESOURCE_TYPE,
    key: "antigravity:user@example.com",
    privateData: {
      accessToken: "access-token",
      refreshToken: "refresh-token",
      displayName: "user@example.com",
      projectId: "project-1",
      quota: null,
    },
    state: { status: "ready" },
  };
  const requests: string[] = [];
  const context: PluginContext = {
    network: {
      fetch: async (url: string) => {
        requests.push(url);
        return {
          status: 200,
          headers: {},
          body: JSON.stringify({
            models: { "gemini-live-model": { displayName: "Gemini Live Model" } },
          }),
        };
      },
      stream: () => Promise.reject(new Error("stream is not expected")),
    },
    signal: new AbortController().signal,
  };
  const models = await antigravityModels.list({ resource }, context);

  assert(models.length === 1, "successful model discovery should replace the fallback catalog");
  assert(models[0].id === "gemini-live-model", "the live model should be returned");
  assert(requests.length === 1, "model discovery should stop after the first successful endpoint");
});

Deno.test("model discovery uses the static catalog when no account is selected", async () => {
  const context: PluginContext = {
    network: {
      fetch: () => Promise.reject(new Error("fetch is not expected")),
      stream: () => Promise.reject(new Error("stream is not expected")),
    },
    signal: new AbortController().signal,
  };

  const models = await antigravityModels.list({ resource: null }, context);
  assert(
    models === STATIC_ANTIGRAVITY_MODELS,
    "static models should remain the no-account fallback",
  );
});
