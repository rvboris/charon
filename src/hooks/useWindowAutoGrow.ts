import { LogicalSize, currentMonitor, getCurrentWindow } from "@tauri-apps/api/window";
import type { RefObject } from "react";
import { useEffect } from "react";

const MINIMUM_HEIGHT = 560;
const SIZE_EPSILON = 0.5;

/**
 * Measures the current window and resizes it towards `target`, bounded by the
 * monitor work area. Returns the height that was actually requested.
 * Module-level so React Compiler does not need to lower its try body.
 */
async function measureAndResize(
  // oxlint-disable-next-line typescript/prefer-readonly-parameter-types -- mutating window API
  win: ReturnType<typeof getCurrentWindow>,
  target: number,
): Promise<void> {
  const [size, scale, monitor] = await Promise.all([
    win.innerSize(),
    win.scaleFactor(),
    currentMonitor(),
  ]);
  const { width, height } = size.toLogical(scale);
  let bounded = Math.max(MINIMUM_HEIGHT, target);
  if (monitor !== null) {
    const maxHeight = monitor.workArea.size.height / scale;
    bounded = Math.min(bounded, maxHeight);
  }
  if (Math.abs(height - bounded) >= SIZE_EPSILON) {
    await win.setSize(new LogicalSize(width, bounded));
  }
}

/* oxlint-disable max-statements, max-lines-per-function, max-depth */

/** Grows/shrinks the window to fit all content, preserving its width. */
export function useWindowAutoGrow(ref: RefObject<HTMLElement | null>): void {
  useEffect((): (() => void) | undefined => {
    const el = ref.current;
    if (el === null) {
      return undefined;
    }

    const win = getCurrentWindow();
    const lifecycle: { disposed: boolean } = { disposed: false };
    let running = false;
    let blocked = false;
    let desired = MINIMUM_HEIGHT;
    let applied = MINIMUM_HEIGHT;
    let firstMeasure = true;

    const pump = async (): Promise<void> => {
      if (running || lifecycle.disposed || blocked) {
        return;
      }
      running = true;
      // Note: `try/finally` and value blocks (conditionals, logical operators)
      // Inside the try body are intentionally avoided — React Compiler cannot
      // Lower them yet. The catch keeps the reset logic below on every path.
      const pending = desired !== applied || firstMeasure;
      try {
        if (pending) {
          const target = desired;
          await measureAndResize(win, target);
          firstMeasure = false;
          // Capped targets settle: never re-pump merely because the uncapped request was larger.
          applied = target;
        }
      } catch {
        blocked = true;
      }
      running = false;
      if (!blocked && desired !== applied) {
        // If a newer desired value arrived while IPC was pending, drain it now.
        void pump();
      }
    };
    const observe = (): void => {
      desired = Math.max(MINIMUM_HEIGHT, el.offsetHeight);
      blocked = false;
      void pump();
    };
    const observer = new ResizeObserver(observe);
    observer.observe(el);
    observe();
    return (): void => {
      lifecycle.disposed = true;
      observer.disconnect();
    };
  }, [ref]);
}
