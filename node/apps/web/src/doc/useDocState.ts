import { useSyncExternalStore } from "react";
import type { DocController, DocState } from "./controller";

const noSubscribe = () => () => {};
const nothing = () => null;

export function useDocState(controller: DocController | null): DocState | null {
  return useSyncExternalStore(controller ? controller.subscribe : noSubscribe, controller ? controller.getState : nothing);
}
