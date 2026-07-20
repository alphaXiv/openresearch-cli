import { useCallback, useEffect, useState } from "react";

export type Bounds = { x: number; y: number; width: number; height: number } | null;

export type TourAnchor = "above" | "below" | "left" | "right";

/**
 * Tracks the bounding box (in viewport coordinates) of one or more elements
 * tagged with `data-onboarding="<selector>"`. Follows them across scroll,
 * resize, and DOM mutations so the tour spotlight stays glued to a target
 * even if it mounts after the step activates. Ported from the openresearch.sh
 * onboarding system; multi-selector union-rect support is retained for parity
 * with it even though every current step tracks a single selector.
 */
export function useTourBounds(selectors: string[]): Bounds {
  const [tracker, setTracker] = useState<null | Tracker>(null);
  const [, setRerender] = useState({});
  // Intentionally render-phase: setTracked diffs by selector and no-ops on
  // repeats, so this is idempotent and never calls setState.
  tracker?.setTracked(selectors);
  useEffect(() => {
    const instance = new Tracker(() => setRerender({}));
    instance.setTracked(selectors);
    setTracker(instance);
    return () => instance.dispose();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return tracker?.bounds ?? null;
}

class Tracker {
  selectors = new Set<string>();
  elements = new Set<Element>();
  onSizeChange = () => this.#onSizeChange();
  resizeObs = new ResizeObserver(this.onSizeChange);
  mutationObs = new MutationObserver((list) => this.onMutation(list));
  bounds: Bounds = null;

  constructor(public onBoundsChange: () => void) {
    window.addEventListener("resize", this.onSizeChange);
    document.addEventListener("scroll", this.onSizeChange, true);
    this.mutationObs.observe(document.body, { childList: true, subtree: true });
  }

  dispose() {
    this.resizeObs.disconnect();
    this.mutationObs.disconnect();
    window.removeEventListener("resize", this.onSizeChange);
    document.removeEventListener("scroll", this.onSizeChange, true);
  }

  setTracked(selectors: string[]) {
    const set = new Set(selectors);
    const newItems = [...set].filter((s) => !this.selectors.has(s));
    const oldItems = [...this.selectors].filter((s) => !set.has(s));
    let changed = false;
    if (newItems.length > 0) {
      const selector = newItems.map((t) => `[data-onboarding="${t}"]`).join(",");
      const matches = document.body.querySelectorAll(selector);
      matches.forEach((el) => {
        this.elements.add(el);
        this.resizeObs.observe(el);
      });
      changed ||= matches.length > 0;
    }
    if (oldItems.length > 0) {
      const selector = oldItems.map((t) => `[data-onboarding="${t}"]`).join(",");
      const matches = document.body.querySelectorAll(selector);
      matches.forEach((el) => {
        this.elements.delete(el);
        this.resizeObs.unobserve(el);
      });
      changed ||= matches.length > 0;
    }
    if (changed) this.#computeBounds();
    this.selectors = set;
  }

  #onSizeChange() {
    const hash = this.#hashBounds();
    this.#computeBounds();
    if (hash !== this.#hashBounds()) this.onBoundsChange();
  }

  #computeBounds() {
    if (this.elements.size === 0) {
      this.bounds = null;
      return;
    }
    const rects = Array.from(this.elements, (el) => el.getBoundingClientRect()).filter(
      (x) => x.width > 0 && x.height > 0,
    );
    if (rects.length === 0) {
      this.bounds = null;
      return;
    }
    const minX = Math.min(...rects.map((r) => r.left));
    const minY = Math.min(...rects.map((r) => r.top));
    const maxX = Math.max(...rects.map((r) => r.right));
    const maxY = Math.max(...rects.map((r) => r.bottom));
    this.bounds = { x: minX, y: minY, width: maxX - minX, height: maxY - minY };
  }

  #hashBounds() {
    return this.bounds
      ? [this.bounds.x, this.bounds.y, this.bounds.width, this.bounds.height].join(",")
      : "null";
  }

  onMutation(mutations: MutationRecord[]) {
    if (this.selectors.size === 0) return;
    const selector = Array.from(this.selectors, (t) => `[data-onboarding="${t}"]`).join(",");
    let changed = false;
    for (const mutation of mutations) {
      for (const node of mutation.addedNodes) {
        if (node instanceof Element) {
          if (node.matches(selector)) {
            this.elements.add(node);
            this.resizeObs.observe(node);
            changed = true;
          }
          node.querySelectorAll(selector).forEach((el) => {
            this.elements.add(el);
            this.resizeObs.observe(el);
            changed = true;
          });
        }
      }
      for (const node of mutation.removedNodes) {
        if (node instanceof Element) {
          if (node.matches(selector)) {
            this.elements.delete(node);
            this.resizeObs.unobserve(node);
            changed = true;
          }
          node.querySelectorAll(selector).forEach((el) => {
            this.elements.delete(el);
            this.resizeObs.unobserve(el);
            changed = true;
          });
        }
      }
    }
    if (changed) this.#onSizeChange();
  }
}

/**
 * Tracks the rendered size of an element via a ref callback, re-rendering when
 * it resizes or its subtree mutates. Used to position the tour card once its
 * real dimensions are known.
 */
export function useMeasure() {
  const [offsetWidth, setWidth] = useState(0);
  const [offsetHeight, setHeight] = useState(0);

  const ref = useCallback((element: HTMLElement | null) => {
    if (!element) return;
    function cb() {
      setWidth(element!.offsetWidth);
      setHeight(element!.offsetHeight);
    }

    const resizeObserver = new ResizeObserver(cb);
    const mutationObserver = new MutationObserver(cb);

    resizeObserver.observe(element);
    mutationObserver.observe(element, {
      childList: true,
      subtree: true,
      characterData: true,
      attributes: true,
    });

    cb();

    return () => {
      resizeObserver.disconnect();
      mutationObserver.disconnect();
    };
  }, []);

  return { ref, offsetWidth, offsetHeight };
}

const VIEWPORT_GAP = 8;

/**
 * Computes the position of a popover so it points at a rectangle from the
 * given side, clamped to the viewport. `arrowAdjustment` is how far the
 * popover was nudged to stay on-screen, so an arrow can shift back to keep
 * pointing at the target. (Named to avoid colliding with the open/close
 * `usePopover` in ModelPicker.tsx.)
 */
export function usePopoverPosition(
  target: {
    x: number;
    y: number;
    width: number;
    height: number;
    anchor: TourAnchor;
    distance: number;
  } | null,
  measure: { offsetWidth: number; offsetHeight: number },
) {
  const { offsetWidth: width, offsetHeight: height } = measure;
  const [{ viewHeight, viewWidth }, setView] = useState({ viewWidth: 0, viewHeight: 0 });

  useEffect(() => {
    function cb() {
      setView({ viewWidth: window.innerWidth, viewHeight: window.innerHeight });
    }
    window.addEventListener("resize", cb);
    cb();
    return () => window.removeEventListener("resize", cb);
  }, []);

  let x = 0;
  let y = 0;
  let arrowAdjustment = 0;

  if (target) {
    const { distance } = target;

    switch (target.anchor) {
      case "left":
        x = target.x - width - distance;
        y = target.y + target.height / 2 - height / 2;
        break;
      case "right":
        x = target.x + target.width + distance;
        y = target.y + target.height / 2 - height / 2;
        break;
      case "below":
        x = target.x + target.width / 2 - width / 2;
        y = target.y + target.height + distance;
        break;
      case "above":
        x = target.x + target.width / 2 - width / 2;
        y = target.y - height - distance;
        break;
    }

    const unclampedX = x;
    const unclampedY = y;
    x = Math.min(Math.max(x, VIEWPORT_GAP), viewWidth - width - VIEWPORT_GAP);
    y = Math.min(Math.max(y, VIEWPORT_GAP), viewHeight - height - VIEWPORT_GAP);

    // How far the clamp moved the card along its cross axis; the arrow shifts
    // back by this amount so it keeps pointing at the target.
    arrowAdjustment =
      target.anchor === "left" || target.anchor === "right" ? unclampedY - y : unclampedX - x;
  }

  return { x, y, arrowAdjustment };
}
