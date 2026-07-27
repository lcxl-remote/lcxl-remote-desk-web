import axios from "axios";
import type { AxiosRequestConfig, AxiosResponse, AxiosError } from "axios";

/**
 * A `RestResponse` business failure (`success === false`).
 *
 * REST endpoints answer with HTTP 200 and signal business outcomes through the
 * envelope, so the machine-readable `code` and any `data` payload only exist in
 * the body. Rejecting with a plain `Error` would keep the message and drop both,
 * leaving callers to match on English text — the error code is what they
 * actually need to localize a message or read the current revision back off a
 * concurrency conflict.
 *
 * Still an `Error` subclass, so existing `instanceof Error` / `error.message`
 * handling is unaffected.
 */
export class RestResponseError extends Error {
    /** `DeskErrorCode` from the `RestResponse` envelope. */
    readonly code: number;
    /** The envelope's `data` payload, if any (e.g. the current revision on conflict). */
    readonly data: unknown;

    constructor(message: string, code: number, data: unknown) {
        super(message);
        this.name = "RestResponseError";
        this.code = code;
        this.data = data;
    }
}

export const axiosInstance = axios.create({});

// Intercept desktop custom RestResponse success=false
axiosInstance.interceptors.response.use(
    (response) => {
        const body = response.data;
        if (body && typeof body === "object" && body.success === false) {
            return Promise.reject(
                new RestResponseError(
                    body.message || "Request failed",
                    typeof body.code === "number" ? body.code : -1,
                    body.data ?? null,
                ),
            );
        }
        return response;
    },
    (error) => {
        return Promise.reject(error);
    }
);

/**
 * Kubb expects a client that matches this signature:
 * <TData, TError, TVariables>(config: RequestConfig<TVariables>) => Promise<Response<TData>>
 */
export const client: Client = <TData = unknown, _TError = unknown, TVariables = unknown>(
    config: RequestConfig<TVariables>
): Promise<Response<TData>> => {
    return axiosInstance.request<TData, Response<TData>, TVariables>(config);
};

export default client;

export type RequestConfig<TData = unknown> = AxiosRequestConfig<TData>;
export type Response<TData = unknown> = AxiosResponse<TData>;
export type ResponseErrorConfig<TError = unknown> = AxiosError<TError>;
export type Client = <TData = unknown, _TError = unknown, TVariables = unknown>(
    config: RequestConfig<TVariables>
) => Promise<Response<TData>>;
