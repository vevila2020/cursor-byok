import type {
  ProviderInvokeInput,
  ProviderOutput,
  ProviderResult,
  ProviderSupport,
} from "cursor-byok:provider";
import type { PluginContext } from "cursor-byok:plugin";
import type { ResourcePatch } from "cursor-byok:resource";
import { HttpError, streamOpenAiChat } from "cursor-byok:protocol/openai-chat";
import { grokModels } from "./models.ts";
import {
  type AccountData,
  accountData,
  accountHeaders,
  quotaExhaustedPatch,
  refreshAccountToken,
  RESOURCE_TYPE,
} from "./resources.ts";

const CHAT_URL = "https://api.x.ai/v1/chat/completions";

/** 流内错误只有文本可用,按积分/额度关键词分类。 */
export function isQuotaError(error: string): boolean {
  const message = error.toLowerCase();
  return message.includes("insufficient_quota") ||
    message.includes("credits exhausted") ||
    message.includes("out of credits") ||
    message.includes("quota_exceeded") ||
    (message.includes("429") &&
      (message.includes("quota") || message.includes("credit") ||
        message.includes("insufficient")));
}

/** HTTP 失败携带结构化状态码,429 一律按额度耗尽处理并冷却账号。 */
function isQuotaHttpError(error: HttpError): boolean {
  if (error.status === 429) return true;
  const body = error.body.toLowerCase();
  return body.includes("insufficient_quota") ||
    body.includes("credits exhausted") ||
    body.includes("out of credits") ||
    // 免费账号触达消费上限时返回 403 spending-limit,属于额度而非授权问题。
    body.includes("spending-limit") ||
    body.includes("run out of credits") ||
    body.includes("quota_exceeded");
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

async function streamRequest(
  data: AccountData,
  modelId: string,
  request: ProviderInvokeInput["request"],
  output: ProviderOutput,
  context: PluginContext,
): Promise<void> {
  await streamOpenAiChat(
    {
      url: CHAT_URL,
      model: modelId,
      // xAI doesn't accept reasoning_effort or service_tier.
      request: {
        ...request,
        reasoning: { enabled: false, effort: null },
        latency: "standard",
      },
      headers: { ...accountHeaders(data), accept: "text/event-stream" },
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
    const httpError = error as HttpError;
    if ((httpError.status === 401 || httpError.status === 403) && !isQuotaHttpError(httpError)) {
      return invalidResult(httpError.message, "Grok authorization expired; sign in again", patch);
    }
    if (isQuotaHttpError(httpError)) {
      return {
        status: "resource-error",
        message: httpError.message,
        patch: quotaExhaustedPatch(data),
      };
    }
    return { status: "request-error", message: httpError.message, ...(patch ? { patch } : {}) };
  }
  const message = error.message;
  if (isQuotaError(message)) {
    return { status: "resource-error", message, patch: quotaExhaustedPatch(data) };
  }
  return { status: "request-error", message, ...(patch ? { patch } : {}) };
}

async function invoke(
  input: ProviderInvokeInput,
  output: ProviderOutput,
  context: PluginContext,
): Promise<ProviderResult> {
  if (!input.resource) {
    return { status: "request-error", message: "add a Grok account before calling Grok" };
  }
  let data: AccountData;
  try {
    data = accountData(input.resource);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return invalidResult(message, message);
  }
  try {
    await streamRequest(data, input.model.id, input.request, output, context);
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
          await streamRequest(refreshedData, input.model.id, input.request, output, context);
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
          "Grok authorization expired; sign in again",
          refreshPatch,
        );
      }
      return { status: "request-error", message: httpError.message };
    }
    const errorValue = error instanceof Error ? error : new Error(String(error));
    return invokeErrorResult(errorValue, data);
  }
}

export const grokProvider: ProviderSupport = {
  id: "grok",
  displayName: "xAI Grok",
  description: {
    "en-US": "SuperGrok subscription access through the official Grok CLI endpoint.",
    "zh-CN": "通过官方 Grok CLI 接口使用 SuperGrok 订阅。",
  },
  providerType: "xai",
  resourceType: RESOURCE_TYPE,
  models: grokModels,
  invoke,
};
