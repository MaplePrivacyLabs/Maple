/**
 * System One: typed decisions with calibrated probabilities (`POST /v1/systemone`).
 *
 * The public surface is re-exported from `./api`; `systemOneWithDependencies`
 * stays here so the package's rolled-up types do not pull the transport
 * internals in.
 */
import { getApiUrl } from "./api";
import {
  openAiAuthenticatedApiCall,
  openAiAuthenticatedApiCallWithDependencies,
  type EncryptedApiDependencies
} from "./encryptedApi";

/** What `true` and `false` mean for a noul question, when the words alone are not enough. */
export type SystemOneNoulCriteria = { true?: unknown; false?: unknown };

/** A yes/no statement, answered with the probability that it is true. */
export type SystemOneNoulQuestion = {
  type: "noul";
  instructions: unknown;
  criteria?: SystemOneNoulCriteria;
};

/**
 * One of 2 to 255 options. Options are numbered in the order of the `criteria`
 * keys (JavaScript keeps insertion order for non-integer keys); each value
 * describes its option (`null` for no description).
 */
export type SystemOneChoiceQuestion = {
  type: "choice";
  instructions: unknown;
  criteria: Record<string, unknown>;
};

/** A level on an ordered scale of 2 to 10 entries, lowest first. */
export type SystemOneScoreQuestion = {
  type: "score";
  instructions: unknown;
  criteria: unknown[];
};

export type SystemOneQuestion =
  SystemOneNoulQuestion | SystemOneChoiceQuestion | SystemOneScoreQuestion;

export type SystemOneRequest = {
  /** Defaults to the only supported model, `glm-5-3-flash`. */
  model?: string;
  /** Any JSON; embedded in the prompt as given. */
  state: unknown;
  /** Up to 64 questions, answered in the order given. */
  questions: Record<string, SystemOneQuestion>;
  /** Up to 4 `data:` image URLs, 4 MiB each and 8 MiB in total, shown with the state. */
  images?: string[];
  /** Calibration temperature override; `1` returns the raw probabilities. */
  temperature?: number;
};

/**
 * Every answer reports the calibration `temperature` applied and `option_mass`,
 * the share of the model's next-token probability that fell on the offered
 * options (low values mean the prompt did not fit the question well).
 */
export type SystemOneNoulAnswer = {
  type: "noul";
  /** Probability that the statement is true. */
  noul: number;
  temperature: number;
  option_mass: number;
};

export type SystemOneChoiceAnswer = {
  type: "choice";
  /** The most probable option's label. */
  choice: string;
  /** Probability per option label, in the order the options were given. */
  probabilities: Record<string, number>;
  /** Peakedness of the distribution, from 0 (uniform) to 1 (certain). */
  confidence: number;
  temperature: number;
  option_mass: number;
};

export type SystemOneScoreAnswer = {
  type: "score";
  /** Expected level number (0-based), a probability-weighted average. */
  score: number;
  /** Level text keyed by level number. */
  legend: Record<string, unknown>;
  /** Probability per level number. */
  probabilities: Record<string, number>;
  confidence: number;
  temperature: number;
  option_mass: number;
};

export type SystemOneAnswer = SystemOneNoulAnswer | SystemOneChoiceAnswer | SystemOneScoreAnswer;

export type SystemOneUsage = {
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  /** Upstream inference requests made: one per question, more for wide choices. */
  requests: number;
};

export type SystemOneResponse = {
  id: string;
  model: string;
  /** One answer per question, keyed by the question's name, in question order. */
  answers: Record<string, SystemOneAnswer>;
  usage: SystemOneUsage;
};

export type SystemOneOptions = {
  /** Authenticate with this API key instead of the signed-in session. */
  apiKey?: string;
};

/**
 * A System One request the backend rejected: `status` is the HTTP status (422
 * for schema problems, 413 for size limits, chat's statuses for quota and
 * capacity) and `code` the `x-opensecret-error-code` value, e.g.
 * `system_one_too_many_questions`.
 */
export class SystemOneError extends Error {
  readonly status: number;
  readonly code?: string;

  constructor(message: string, status: number, code?: string) {
    super(message);
    this.name = "SystemOneError";
    this.status = status;
    this.code = code;
  }
}

/**
 * Answers typed System One questions about a state with calibrated
 * probabilities (`POST /v1/systemone`, Continuum GLM-5.3-Flash). Uses the
 * signed-in session unless `options.apiKey` is given. Rejections throw
 * {@link SystemOneError}.
 */
export async function systemOne(
  request: SystemOneRequest,
  options?: SystemOneOptions
): Promise<SystemOneResponse> {
  return systemOneWithDependencies(request, options, undefined);
}

/** @internal Exported for deterministic transport tests, not from the package entry point. */
export async function systemOneWithDependencies(
  request: SystemOneRequest,
  options: SystemOneOptions | undefined,
  dependencies: EncryptedApiDependencies | undefined
): Promise<SystemOneResponse> {
  const url = `${dependencies ? dependencies.getApiUrl() : getApiUrl()}/v1/systemone`;
  try {
    if (dependencies) {
      return await openAiAuthenticatedApiCallWithDependencies<SystemOneRequest, SystemOneResponse>(
        url,
        "POST",
        request,
        "System One request failed",
        options?.apiKey,
        dependencies
      );
    }
    return await openAiAuthenticatedApiCall<SystemOneRequest, SystemOneResponse>(
      url,
      "POST",
      request,
      "System One request failed",
      options?.apiKey
    );
  } catch (error) {
    throw systemOneError(error);
  }
}

/**
 * Only a backend response becomes a {@link SystemOneError}; transport and
 * session failures are rethrown as the SDK reports them.
 */
function systemOneError(error: unknown): unknown {
  if (!(error instanceof Error)) return error;
  const { status, headers } = error as { status?: unknown; headers?: unknown };
  if (typeof status !== "number" || !(headers instanceof Headers)) return error;
  return new SystemOneError(
    error.message,
    status,
    headers.get("x-opensecret-error-code") ?? undefined
  );
}
