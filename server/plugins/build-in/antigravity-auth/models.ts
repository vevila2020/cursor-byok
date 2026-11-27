import type { PluginContext } from "cursor-byok:plugin";
import type {
  ModelDefinition,
  ModelListInput,
  ModelSnapshot,
  ModelSupport,
} from "cursor-byok:model";
import { accountData } from "./resources.ts";

export const ANTIGRAVITY_PROD_ENDPOINT = "https://cloudcode-pa.googleapis.com";
export const ANTIGRAVITY_DAILY_ENDPOINT = "https://daily-cloudcode-pa.googleapis.com";
export const ANTIGRAVITY_SANDBOX_ENDPOINT = "https://daily-cloudcode-pa.sandbox.googleapis.com";

export const ANTIGRAVITY_ENDPOINTS = [
  ANTIGRAVITY_DAILY_ENDPOINT,
  ANTIGRAVITY_PROD_ENDPOINT,
  ANTIGRAVITY_SANDBOX_ENDPOINT,
];

const FETCH_AVAILABLE_MODELS_PATH = "/v1internal:fetchAvailableModels";
// The CLI product identity unlocks the Gemini 4 catalog. The build-specific
// "cl" value that the CLI also sends is deliberately omitted: it gates only the
// orchestrator aliases and would need syncing with every CLI build.
export const ANTIGRAVITY_USER_AGENT = "antigravity/cli/1.5.18";

export const ANTIGRAVITY_CLIENT_HEADERS: Record<string, string> = {};

const ANTIGRAVITY_DENYLIST = new Set([
  "chat_20706",
  "chat_23310",
  "tab_flash_lite_preview",
  "tab_jump_flash_lite_preview",
]);

function staticModel(
  id: string,
  displayName: string,
  images = true,
  maxOutputTokens = 65_536,
): ModelDefinition {
  return {
    id,
    displayName,
    capabilities: { images },
    maxOutputTokens,
    privateData: { reasoningEfforts: [] },
  };
}

export const STATIC_ANTIGRAVITY_MODELS: ModelDefinition[] = [
  staticModel("gemini-4-neon-high", "Gemini 4 Neon (High)", true, 262_144),
  staticModel("gemini-4-neon-xhigh", "Gemini 4 Neon (XHigh)", true, 262_144),
  staticModel("gemini-4-neon-medium", "Gemini 4 Neon (Medium)", true, 262_144),
  staticModel("gemini-4-neon-low", "Gemini 4 Neon (Low)", true, 262_144),
  staticModel("gemini-4-carbon-medium", "Gemini 4 Carbon (Medium)", true, 262_144),
  staticModel("gemini-4-carbon-high", "Gemini 4 Carbon (High)", true, 262_144),
  staticModel("gemini-4-carbon-low", "Gemini 4 Carbon (Low)", true, 262_144),
  staticModel("gemini-4-argon-medium", "Gemini 4 Argon (Medium)"),
  staticModel("gemini-4-argon-high", "Gemini 4 Argon (High)"),
  staticModel("gemini-4-argon-low", "Gemini 4 Argon (Low)"),
  staticModel("gemini-4-hydrogen-high", "Gemini 4 Hydrogen (High)", true, 262_144),
  staticModel("gemini-4-hydrogen-xhigh", "Gemini 4 Hydrogen (XHigh)", true, 262_144),
  staticModel("gemini-4-hydrogen-medium", "Gemini 4 Hydrogen (Medium)", true, 262_144),
  staticModel("gemini-4-hydrogen-low", "Gemini 4 Hydrogen (Low)", true, 262_144),
  staticModel("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
  staticModel("gemini-3.8-flash-medium", "Gemini 3.8 Flash (Medium)"),
  staticModel("gemini-3.8-flash-low", "Gemini 3.8 Flash (Low)"),
  staticModel("claude-opus-5-5-low", "Claude Opus 5.5 (Low)", true, 128_000),
  staticModel("claude-opus-5-5-medium", "Claude Opus 5.5 (Medium)", true, 128_000),
  staticModel("claude-opus-5-5-high", "Claude Opus 5.5 (High)", true, 128_000),
  staticModel("claude-sonnet-5-5-low", "Claude Sonnet 5.5 (Low)", true, 128_000),
  staticModel("claude-sonnet-5-5-medium", "Claude Sonnet 5.5 (Medium)", true, 128_000),
  staticModel("claude-sonnet-5-5-high", "Claude Sonnet 5.5 (High)", true, 128_000),
  staticModel("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)", false, 32_768),
];

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

function logModelSync(details: Record<string, unknown>): void {
  console.error(`[antigravity model sync] ${JSON.stringify(details)}`);
}

export function parseAntigravityModels(payload: unknown): ModelDefinition[] {
  const root = object(payload);
  const rawModels = object(root?.models);
  if (!rawModels) return [];

  const models: ModelDefinition[] = [];
  const seen = new Set<string>();

  for (const [modelId, raw] of Object.entries(rawModels)) {
    if (ANTIGRAVITY_DENYLIST.has(modelId)) continue;
    const model = object(raw);
    if (!model) continue;

    const id = modelId.trim();
    if (!id || seen.has(id)) continue;
    seen.add(id);

    const displayName = text(model.displayName) ?? id;
    const supportsThinking = model.supportsThinking === true;
    const reasoningEfforts = supportsThinking ? ["low", "medium", "high"] : [];
    const maxOutputTokens = typeof model.maxOutputTokens === "number" && model.maxOutputTokens > 0
      ? model.maxOutputTokens
      : 65_536;

    models.push({
      id,
      displayName,
      capabilities: {
        images: model.supportsImages === true || id.includes("gemini") || id.includes("claude"),
      },
      maxOutputTokens,
      privateData: { reasoningEfforts },
    });
  }

  return models;
}

function agentModelSortIds(response: Record<string, unknown>): string[] {
  const sorts = response.agentModelSorts;
  if (!Array.isArray(sorts)) return [];

  const ids = new Set<string>();
  for (const sort of sorts) {
    const groups = object(sort)?.groups;
    if (!Array.isArray(groups)) continue;
    for (const group of groups) {
      const modelIds = object(group)?.modelIds;
      if (!Array.isArray(modelIds)) continue;
      for (const modelId of modelIds) {
        if (typeof modelId === "string" && modelId.trim()) ids.add(modelId.trim());
      }
    }
  }

  return [...ids];
}

export function selectAntigravityModels(payload: unknown): ModelDefinition[] {
  const response = object(payload);
  if (!response) return [];

  const modelsById = new Map(parseAntigravityModels(payload).map((model) => [model.id, model]));
  const recommendedIds = agentModelSortIds(response);
  if (recommendedIds.length === 0) return [...modelsById.values()];

  return recommendedIds.map((id) => modelsById.get(id) ?? staticModel(id, id, false));
}

export function reasoningEfforts(model: ModelSnapshot): string[] {
  const data = object(model.privateData);
  const efforts = data?.reasoningEfforts;
  return Array.isArray(efforts) ? efforts.filter((item) => typeof item === "string") : [];
}

export const antigravityModels: ModelSupport = {
  refreshResourceBeforeList: true,
  list: async (
    { resource }: ModelListInput,
    context: PluginContext,
  ): Promise<ModelDefinition[]> => {
    if (!resource) {
      logModelSync({ event: "fallback", reason: "no account resource" });
      return STATIC_ANTIGRAVITY_MODELS;
    }
    let data;
    try {
      data = accountData(resource);
    } catch (error) {
      logModelSync({
        event: "fallback",
        reason: "invalid account resource",
        error: error instanceof Error ? error.message : String(error),
      });
      return STATIC_ANTIGRAVITY_MODELS;
    }

    const projectId = data.projectId || "bamboo-precept-lgxtn";
    const requests = [
      {
        projectMode: "account-project",
        body: JSON.stringify({ project: projectId }),
      },
      { projectMode: "empty-project", body: JSON.stringify({}) },
    ];
    for (const endpoint of ANTIGRAVITY_ENDPOINTS) {
      for (const request of requests) {
        try {
          const response = await context.network.fetch(
            `${endpoint}${FETCH_AVAILABLE_MODELS_PATH}`,
            {
              method: "POST",
              headers: {
                authorization: `Bearer ${data.accessToken}`,
                "content-type": "application/json",
                "user-agent": ANTIGRAVITY_USER_AGENT,
                ...ANTIGRAVITY_CLIENT_HEADERS,
              },
              body: request.body,
            },
          );

          if (response.status < 200 || response.status >= 300) {
            logModelSync({
              event: "http-error",
              endpoint,
              projectMode: request.projectMode,
              projectId: request.projectMode === "account-project" ? projectId : undefined,
              status: response.status,
            });
            continue;
          }

          const body = JSON.parse(response.body);
          const responseBody = object(body) ?? {};
          const rawModels = object(responseBody.models) ?? {};
          const groupedModels = object(responseBody.groupedModels) ??
            object(responseBody.grouped_models) ?? {};
          const tieredModelIdsValue = responseBody.tieredModelIds ??
            responseBody.tiered_model_ids;
          const tieredModelIds = Array.isArray(tieredModelIdsValue)
            ? tieredModelIdsValue.filter((id): id is string => typeof id === "string")
            : [];
          const parsedModels = parseAntigravityModels(body);
          const models = selectAntigravityModels(body);
          logModelSync({
            event: "model-list-response",
            endpoint,
            projectMode: request.projectMode,
            projectId: request.projectMode === "account-project" ? projectId : undefined,
            status: response.status,
            responseFields: Object.keys(responseBody),
            groupedModelIds: Object.keys(groupedModels),
            tieredModelIds,
            defaultAgentModelId: text(responseBody.defaultAgentModelId),
            modelMetadata: {
              agentModelSorts: responseBody.agentModelSorts,
              commandModelIds: responseBody.commandModelIds,
              tabModelIds: responseBody.tabModelIds,
              imageGenerationModelIds: responseBody.imageGenerationModelIds,
              mqueryModelIds: responseBody.mqueryModelIds,
              webSearchModelIds: responseBody.webSearchModelIds,
              commitMessageModelIds: responseBody.commitMessageModelIds,
              audioTranscriptionModelIds: responseBody.audioTranscriptionModelIds,
              battleModeModelSorts: responseBody.battleModeModelSorts,
            },
            rawModels: Object.entries(rawModels).map(([id, value]) => ({
              id,
              displayName: text(object(value)?.displayName),
            })),
            parsedModelCount: parsedModels.length,
            selectedModelCount: models.length,
          });
          if (models.length > 0) {
            logModelSync({
              event: "selected-catalog",
              endpoint,
              projectMode: request.projectMode,
              modelCount: models.length,
              modelIds: models.map((model) => model.id),
            });
            return models;
          }
        } catch (error) {
          logModelSync({
            event: "request-error",
            endpoint,
            projectMode: request.projectMode,
            projectId: request.projectMode === "account-project" ? projectId : undefined,
            error: error instanceof Error ? error.message : String(error),
          });
        }
      }
    }

    logModelSync({ event: "fallback", reason: "no dynamic models returned" });
    return STATIC_ANTIGRAVITY_MODELS;
  },
};
