import axios from "axios";
import type { AxiosRequestConfig, AxiosResponse, AxiosError, AxiosInstance } from "axios";

export const axiosInstance = axios.create({});

// Intercept desktop custom RestResponse success=false
axiosInstance.interceptors.response.use(
    (response) => {
        if (response.data && typeof response.data === "object" && response.data.success === false) {
            return Promise.reject(new Error(response.data.message || "Request failed"));
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
export type Client = <TData = unknown, TError = unknown, TVariables = unknown>(
    config: RequestConfig<TVariables>
) => Promise<Response<TData>>;
