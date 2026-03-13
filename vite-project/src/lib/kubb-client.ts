import axios from "axios";
import type { AxiosRequestConfig, AxiosResponse, AxiosError, AxiosInstance } from "axios";

export const client = axios.create({});

// Intercept desktop custom RestResponse success=false
client.interceptors.response.use(
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

export default client;

export type RequestConfig<TData = unknown> = Record<string, any>;
export type Response<TData = unknown> = AxiosResponse<TData>;
export type ResponseErrorConfig<TError = unknown> = AxiosError<TError>;
export type { AxiosInstance as Client };
