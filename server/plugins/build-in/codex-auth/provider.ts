import type {
  ProviderInvokeInput,
  ProviderOutput,
  ProviderResult,
  ProviderSupport,
} from "cursor-byok:provider";
import type { PluginContext } from "cursor-byok:plugin";
import type { ResourcePatch } from "cursor-byok:resource";
import { HttpError, streamOpenAiResponses } from "cursor-byok:protocol/openai-responses";
import { codexModels, reasoningEfforts } from "./models.ts";
import {
  type AccountData,
  accountData,
  chatGptAccountId,
  quotaExhaustedPatch,
  refreshAccountToken,
  RESOURCE_TYPE,
} from "./resources.ts";

const RESPONSES_URL = "https://chatgpt.com/backend-api/codex/responses";

/** 流内错误只有文本可用,按额度关键词分类。 */
export function isQuotaError(error: string): boolean {
  const message = error.toLowerCase();
  return message.includes("insufficient_quota") ||
    message.includes("credit_balance_exhausted") ||
    message.includes("usage_limit_reached") ||
    message.includes("exceeded your current quota") ||
    message.includes("quota_exceeded") ||
    message.includes("5-hour") ||
    message.includes("5 hour") ||
    (message.includes("429") &&
      (message.includes("quota") || message.includes("usage_limit") ||
        message.includes("insufficient")));
}

/** HTTP 失败携带结构化状态码,429 时放宽响应体的匹配条件。 */
function isQuotaHttpError(error: HttpError): boolean {
  const body = error.body.toLowerCase();
  return body.includes("insufficient_quota") ||
    body.includes("usage_limit_reached") ||
    body.includes("exceeded your current quota") ||
    body.includes("quota_exceeded") ||
    body.includes("5-hour") ||
    body.includes("5 hour") ||
    (error.status === 429 &&
      (body.includes("quota") || body.includes("usage_limit") || body.includes("insufficient")));
}

function invalidResult(
  message: string,
  stateMessage: string,
  patch?: ResourcePatch,
): ProviderResult {
  return {
    status: "resource-error",
    message,
    patch: { ...patch, state: { status: "invalid", message: stateMessage } },
  };
}

function headers(data: AccountData, cacheKey: string | null): Record<string, string> {
  const result: Record<string, string> = {
    authorization: `Bearer ${data.accessToken}`,
    originator: "codex_cli_rs",
  };
  const accountId = chatGptAccountId(data.accessToken);
  if (accountId) result["ChatGPT-Account-Id"] = accountId;
  // Codex 后端的缓存亲和契约:session-id / thread-id / prompt_cache_key
  // 三者同源(见 codex-rs client.rs);缺头会导致请求落在随机分片上。
  if (cacheKey !== null) {
    result["session-id"] = cacheKey;
    result["thread-id"] = cacheKey;
    result["x-client-request-id"] = cacheKey;
  }
  return result;
}

async function streamRequest(
  data: AccountData,
  modelId: string,
  request: ProviderInvokeInput["request"],
  effort: string | null,
  output: ProviderOutput,
  context: PluginContext,
): Promise<void> {
  await streamOpenAiResponses(
    {
      url: RESPONSES_URL,
      model: modelId,
      // Codex subscription endpoint doesn't accept max_output_tokens; fast maps to service_tier=priority.
      request: {
        ...request,
        reasoning: { enabled: request.reasoning.enabled, effort },
        maxOutputTokens: null,
      },
      headers: headers(data, request.cacheKey),
      extraBody: { store: false },
    },
    output,
    context,
  );
}

function invokeErrorResult(
  error: Error,
  data: AccountData,
  patch?: ResourcePatch,
): ProviderResult {
  if (error instanceof HttpError) {
    if (error.status === 401) {
      return invalidResult(error.message, "ChatGPT authorization expired; sign in again", patch);
    }
    if (isQuotaHttpError(error)) {
      return {
        status: "resource-error",
        message: error.message,
        patch: quotaExhaustedPatch(data, error.body),
      };
    }
    return { status: "request-error", message: error.message, ...(patch ? { patch } : {}) };
  }
  const message = error.message;
  if (isQuotaError(message)) {
    return { status: "resource-error", message, patch: quotaExhaustedPatch(data, message) };
  }
  return { status: "request-error", message, ...(patch ? { patch } : {}) };
}

async function invoke(
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<ProviderResult> {
  if (!input.resource) {
    return { status: "request-error", message: "add a ChatGPT account before calling Codex" };
  }
  let data: AccountData;
  try {
    data = accountData(input.resource);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return invalidResult(message, message);
  }
  const efforts = reasoningEfforts(input.model);
  const reasoning = input.request.reasoning;
  const effort = reasoning.effort !== null && efforts.includes(reasoning.effort)
    ? reasoning.effort
    : null;
  try {
    await streamRequest(data, input.model.id, input.request, effort, output, context);
    return { status: "completed" };
  } catch (error) {
    const httpError = error instanceof HttpError ? error as HttpError : null;
    if (httpError?.status === 401 && data.refreshToken) {
      const refreshPatch = await refreshAccountToken(input.resource, context);
      if (refreshPatch.privateData) {
        const refreshedData = accountData({
          ...input.resource,
          privateData: refreshPatch.privateData,
        });
        try {
          await streamRequest(
            refreshedData,
            input.model.id,
            input.request,
            effort,
            output,
            context,
          );
          return { status: "completed", patch: refreshPatch };
        } catch (retryError) {
          const retryErrorValue = retryError instanceof Error
            ? retryError
            : new Error(String(retryError));
          return invokeErrorResult(retryErrorValue, refreshedData, refreshPatch);
        }
      }
      if (refreshPatch.state?.status === "invalid") {
        return invalidResult(
          httpError.message,
          "ChatGPT authorization expired; sign in again",
          refreshPatch,
        );
      }
      return { status: "request-error", message: httpError.message };
    }
    const errorValue = error instanceof Error ? error : new Error(String(error));
    return invokeErrorResult(errorValue, data);
  }
}

export const codexProvider: ProviderSupport = {
  id: "codex",
  displayName: "OpenAI Codex",
  description: {
    "en-US": "ChatGPT subscription access through the official Codex Responses API.",
    "zh-CN": "通过官方 Codex Responses API 使用 ChatGPT 订阅。",
  },
  providerType: "openai",
  resourceType: RESOURCE_TYPE,
  models: codexModels,
  invoke,
};
