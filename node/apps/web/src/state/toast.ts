export type ToastTone = "info" | "ok" | "error";
export type ToastFn = (message: string, tone?: ToastTone) => void;
