import axios from "axios";
import type { AxiosRequestConfig, AxiosResponse, AxiosError, AxiosInstance } from "axios";

export const client = axios.create({
    baseURL: "/api",
});

export default client;

export type RequestConfig<TData = unknown> = Record<string, any>;
export type Response<TData = unknown> = AxiosResponse<TData>;
export type ResponseErrorConfig<TError = unknown> = AxiosError<TError>;
export type { AxiosInstance as Client };
